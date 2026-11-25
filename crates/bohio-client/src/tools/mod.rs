//! Agent tools: what an agent does on a host, as typed calls with JSON arguments and results.
//!
//! [`Tools`] is transport-neutral: it works over any [`Open`], and an embedding agent (or the `bohio mcp` server) calls it
//! in process. Every tool is one [`ToolSpec`] in [`REGISTRY`]: a name, a description, a JSON Schema for its arguments, the
//! permissions it needs and a function; a new group of calls is a new entry and nothing else. [`Tools::call`] takes the
//! arguments as JSON and answers JSON (and images), so a protocol server maps one to the other without knowing a tool.
//!
//! Nothing here returns an unbounded answer: every size is a [`Settings`] field, and what a bound cut off is said in the
//! result (`truncated`, `omitted_bytes`, `next_*`), never dropped silently. Binary content is `base64`. A tool the peer
//! may not use is refused with the reason, by what `info` says of the host and by what the host answers.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError};
use std::time::Duration;

use bohio_proto::ErrorCode;
use bohio_proto::info::Info;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

use crate::forward::ForwardSettings;
use crate::{Error, Open};

mod discover;
mod files;
mod net;
mod process;
mod registry;
mod screen;
mod system;

pub use registry::REGISTRY;
pub use screen::{
    CursorPosition, DisplayInfo, Displays, Done, Pointer, Screenshot, ScreenshotOptions,
    ShotFormat, Space, Waited,
};

/// How big a call's answers may be: what a call gets when it names nothing, and the most it may ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bound {
    /// What a call that names no size gets.
    pub default: u64,
    /// The most a call may ask for; a larger request is lowered to it and the answer says so.
    pub ceiling: u64,
}

impl Bound {
    /// The size for a call that asked for `asked`.
    #[must_use]
    pub fn pick(&self, asked: Option<u64>) -> u64 {
        asked.map_or(self.default, |value| value.min(self.ceiling))
    }
}

/// The bounds and waits of the tools. Every field has a strict default; a caller widens what its agent needs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Settings {
    /// How long a call waits for the host's first answer. Default 30 s.
    pub reply_timeout: Duration,
    /// How long one step of a transfer waits for the host; `None` waits without limit. Default 5 minutes.
    pub transfer_timeout: Option<Duration>,
    /// Bytes of one output stream a call returns (exec's standard output, a session read, a file read). Default 64 KiB,
    /// ceiling 4 MiB.
    pub output: Bound,
    /// Lines of a text read. Default 2000, ceiling 100 000.
    pub lines: Bound,
    /// Entries of one list or walk page. Default 200 and 1000, ceiling 10 000 and 100 000.
    pub list: Bound,
    /// See [`Settings::list`]: the walk's.
    pub walk: Bound,
    /// How long a `follow` watches a file, in milliseconds. Default 5 s, ceiling 5 minutes.
    pub follow: Bound,
    /// Bytes of all the files of one `read_many`. Default 1 MiB, ceiling 16 MiB.
    pub bundle: Bound,
    /// The largest file `edit` reads and the largest text `write` and `write_many` send, in bytes. Default 8 MiB.
    pub max_text: u64,
    /// The deadline of an `exec` that names none, in milliseconds; `0` is none. Default 120 s.
    pub exec_deadline_ms: u64,
    /// How long past its deadline an `exec` waits for the host to report the end before it gives up. Default 15 s.
    pub exec_grace: Duration,
    /// How often a waiting call looks at its cancellation. Default 50 ms.
    pub poll: Duration,
    /// How long a session read waits for more output after the last, in milliseconds. Default 300.
    pub session_quiet_ms: u64,
    /// Directories of this machine that `put` and `get` may use; with none, those tools are unavailable. Default none.
    pub local_roots: Vec<PathBuf>,
    /// Targets of this machine (`HOST:PORT`) a remote forward may reach; with none, `forward_remote` is unavailable.
    /// Default none.
    pub forward_local_targets: Vec<String>,
    /// Whether a local forward may listen beyond loopback (whoever reaches it has the peer's grant). Default false.
    pub forward_allow_remote_bind: bool,
    /// The most forwards held at once. Default 16.
    pub max_forwards: usize,
    /// The waits of a forward.
    pub forward: ForwardSettings,
    /// How long a desktop call waits for an answer (the host may wait for its owner to allow the session). Default 120 s.
    pub gui_timeout: Duration,
    /// The largest image a screenshot returns, in bytes; a larger one is refused as `too_large`. Default 4 MiB.
    pub gui_max_image: u64,
    /// Where `format: "auto"` turns to JPEG: a PNG screenshot larger than this many bytes is taken again as JPEG.
    /// Default 1 MiB. A lower value spares the link, a higher one keeps exact pixels for longer.
    pub gui_auto_limit: u64,
    /// The JPEG quality of `auto` and of a `jpeg` screenshot that names none, 1 to 100. Default 80.
    pub gui_jpeg_quality: u8,
    /// The width a screenshot that names none fits, in pixels (the host scales down, never up). Default 1280.
    pub gui_max_width: u32,
    /// The height a screenshot that names none fits, in pixels. Default 800.
    pub gui_max_height: u32,
    /// Whether every input call answers with a settled screenshot of the display it acted on. Default false. It costs a
    /// snapshot a call, and an input-only grant (no `gui.view`) gets the input and a `screenshot_error`.
    pub gui_after_action: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            reply_timeout: Duration::from_secs(30),
            transfer_timeout: Some(Duration::from_secs(300)),
            output: Bound {
                default: 64 * 1024,
                ceiling: 4 * 1024 * 1024,
            },
            lines: Bound {
                default: 2000,
                ceiling: 100_000,
            },
            list: Bound {
                default: 200,
                ceiling: 10_000,
            },
            walk: Bound {
                default: 1000,
                ceiling: 100_000,
            },
            follow: Bound {
                default: 5000,
                ceiling: 300_000,
            },
            bundle: Bound {
                default: 1024 * 1024,
                ceiling: 16 * 1024 * 1024,
            },
            max_text: 8 * 1024 * 1024,
            exec_deadline_ms: 120_000,
            exec_grace: Duration::from_secs(15),
            poll: Duration::from_millis(50),
            session_quiet_ms: 300,
            local_roots: Vec::new(),
            forward_local_targets: Vec::new(),
            forward_allow_remote_bind: false,
            max_forwards: 16,
            forward: ForwardSettings::default(),
            gui_timeout: Duration::from_secs(120),
            gui_max_image: 4 * 1024 * 1024,
            gui_auto_limit: 1024 * 1024,
            gui_jpeg_quality: 80,
            gui_max_width: 1280,
            gui_max_height: 800,
            gui_after_action: false,
        }
    }
}

/// A flag a caller sets to end the call that holds it: a running `exec` is killed, a read or a watch stops.
#[derive(Debug, Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    /// A flag not set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets it.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    /// Whether it is set.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Why a tool did not do what it was asked.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolError {
    /// A short name: the host's error code (`denied`, `not_found`, `changed`, ...) or one of this module's
    /// (`bad_request`, `not_text`, `no_match`, `ambiguous`, `unavailable`, `unknown_tool`, `timeout`, `closed`, `link`, and for
    /// the desktop `out_of_bounds`, `no_screenshot`, `stale_screenshot`).
    pub code: String,
    /// What happened, for the agent.
    pub message: String,
    /// More, as JSON, when the code has some (a match count, a size).
    pub detail: Option<Value>,
}

impl ToolError {
    /// An error of `code`.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            detail: None,
        }
    }

    /// A malformed call.
    pub fn bad(message: impl Into<String>) -> Self {
        Self::new("bad_request", message)
    }

    /// With `detail`.
    #[must_use]
    pub fn with_detail(mut self, detail: Value) -> Self {
        self.detail = Some(detail);
        self
    }
}

impl core::fmt::Display for ToolError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for ToolError {}

impl From<Error> for ToolError {
    fn from(error: Error) -> Self {
        match error {
            Error::Remote(body) => {
                let code = serde_json::to_value(body.code)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_else(|| "other".to_owned());
                Self::new(code, body.message)
            }
            Error::Transport(coqui_transport::Failure::Permission) => Self::new(
                "denied",
                "the host refused or ended the stream: this peer may not do that, or its access was revoked",
            ),
            Error::Timeout => Self::new("timeout", "the host did not answer in time"),
            Error::Closed => Self::new(
                "closed",
                "the stream or the link ended before the operation finished",
            ),
            Error::Link(text) => Self::new("link", text),
            Error::Mismatch => Self::new(
                "mismatch",
                "the rebuilt file does not match the digest the host reported; nothing was put in place",
            ),
            other => Self::new("failed", other.to_string()),
        }
    }
}

impl From<bohio_proto::ErrorBody> for ToolError {
    fn from(body: bohio_proto::ErrorBody) -> Self {
        Error::Remote(body).into()
    }
}

/// What a tool answers: JSON, and the images that go with it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Output {
    /// The result.
    pub value: Value,
    /// Images (a snapshot), each with its media type.
    pub images: Vec<Image>,
}

/// An encoded image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// Its media type (`image/png`).
    pub mime: String,
    /// The encoded bytes.
    pub data: Vec<u8>,
}

impl Output {
    pub(crate) fn json(value: &impl Serialize) -> Result<Self, ToolError> {
        Ok(Self {
            value: serde_json::to_value(value)
                .map_err(|_| ToolError::new("internal", "a result could not be written"))?,
            images: Vec::new(),
        })
    }
}

/// What a tool needs to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Need {
    /// The peer's `info` permission is what a tool wants to read the host's capabilities; a tool does not need it.
    Info,
    /// `fs.read`.
    FsRead,
    /// `fs.write`.
    FsWrite,
    /// `exec`.
    Exec,
    /// `session`.
    Session,
    /// `power`, and a power source the node serves.
    Power,
    /// `media`, and a media source the node serves.
    Media,
    /// `forward.connect`.
    ForwardConnect,
    /// `forward.listen`.
    ForwardListen,
    /// `gui.view`.
    GuiView,
    /// `gui.input`.
    GuiInput,
    /// `gui.clipboard`.
    GuiClipboard,
    /// A directory of this machine the embedder allows ([`Settings::local_roots`]).
    LocalRoots,
    /// A target of this machine a remote forward may reach ([`Settings::forward_local_targets`]).
    LocalTargets,
}

impl Need {
    /// The permission's name as `bohio.cfg` writes it, or the setting's.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::FsRead => "fs.read",
            Self::FsWrite => "fs.write",
            Self::Exec => "exec",
            Self::Session => "session",
            Self::Power => "power",
            Self::Media => "media",
            Self::ForwardConnect => "forward.connect",
            Self::ForwardListen => "forward.listen",
            Self::GuiView => "gui.view",
            Self::GuiInput => "gui.input",
            Self::GuiClipboard => "gui.clipboard",
            Self::LocalRoots => "local roots",
            Self::LocalTargets => "forward local targets",
        }
    }
}

/// What a tool does, for a client that asks before it acts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Effect {
    /// Changes nothing on the host or here.
    ReadOnly,
    /// Changes files, or opens a forward, but runs no program.
    Changes,
    /// Starts programs or drives the desktop.
    Runs,
}

impl ToolSpec {
    /// Whether the tool looks at or drives the host's desktop. A server lists these only when its operator asks for them.
    #[must_use]
    pub fn is_gui(&self) -> bool {
        self.needs
            .iter()
            .any(|need| matches!(need, Need::GuiView | Need::GuiInput | Need::GuiClipboard))
    }
}

/// A tool's entry point: the arguments as JSON.
pub type Runner = fn(&Tools, Value, &Cancel) -> Result<Output, ToolError>;

/// One tool.
#[derive(Debug, Clone, Copy)]
pub struct ToolSpec {
    /// The name a client calls it by.
    pub name: &'static str,
    /// What it does and how an agent should use it.
    pub description: &'static str,
    /// The JSON Schema of its arguments.
    pub schema: fn() -> Value,
    /// What the peer and this side must have for it to work.
    pub needs: &'static [Need],
    /// What it changes.
    pub effect: Effect,
    /// The function.
    pub run: Runner,
}

/// Whether a tool can run for this peer on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// It can, as far as is known.
    Available,
    /// It cannot, and why.
    Unavailable(String),
}

/// The tools over one link to a host.
pub struct Tools {
    pub(crate) link: Arc<dyn Open>,
    pub(crate) info: Option<Info>,
    pub(crate) settings: Settings,
    pub(crate) forwards: Mutex<net::Held>,
    pub(crate) desktop: Mutex<Option<screen::Held>>,
    pub(crate) shots: Mutex<screen::Shots>,
}

impl core::fmt::Debug for Tools {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Tools").finish_non_exhaustive()
    }
}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Tools {
    /// The tools over `link`, with the default [`Settings`]; reads the host's `info` once.
    ///
    /// A peer without the `info` permission is not an error: the tools then try and report what the host answers.
    ///
    /// # Errors
    ///
    /// The link's, when it cannot ask at all.
    pub fn new(link: Arc<dyn Open>) -> Result<Self, Error> {
        Self::with_settings(link, Settings::default())
    }

    /// As [`Tools::new`], with `settings`.
    ///
    /// # Errors
    ///
    /// As [`Tools::new`].
    pub fn with_settings(link: Arc<dyn Open>, settings: Settings) -> Result<Self, Error> {
        let control = crate::control::Control::open(&*link)?;
        let info = match control.info(Some(settings.reply_timeout)) {
            Ok(info) => Some(info),
            Err(Error::Remote(body))
                if matches!(body.code, ErrorCode::Denied | ErrorCode::Unsupported) =>
            {
                None
            }
            Err(Error::Transport(coqui_transport::Failure::Permission)) => None,
            Err(error) => return Err(error),
        };
        control.close();
        Ok(Self {
            link,
            info,
            settings,
            forwards: Mutex::new(net::Held::default()),
            desktop: Mutex::new(None),
            shots: Mutex::new(screen::Shots::new()),
        })
    }

    /// The host's capabilities as read when these tools were made; `None` for a peer that may not ask.
    #[must_use]
    pub const fn info(&self) -> Option<&Info> {
        self.info.as_ref()
    }

    /// The settings.
    #[must_use]
    pub const fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Whether `spec` can run, by what the host reported and what this side allows.
    #[must_use]
    pub fn availability(&self, spec: &ToolSpec) -> Availability {
        for need in spec.needs {
            if let Some(reason) = self.lacks(*need) {
                return Availability::Unavailable(format!(
                    "{reason} (the tool needs {})",
                    need.name()
                ));
            }
        }
        Availability::Available
    }

    fn lacks(&self, need: Need) -> Option<String> {
        match need {
            Need::LocalRoots => self
                .settings
                .local_roots
                .is_empty()
                .then(|| "no directory of this machine is allowed for it".to_owned()),
            Need::LocalTargets => self
                .settings
                .forward_local_targets
                .is_empty()
                .then(|| "no local target is allowed for a remote forward".to_owned()),
            _ => {
                let info = self.info.as_ref()?;
                let off = |on: bool, what: &str| {
                    (!on).then(|| format!("the host has {what} turned off or not offered"))
                };
                match need {
                    Need::FsRead | Need::FsWrite if info.access.fs.is_empty() => {
                        Some("this node serves no files".to_owned())
                    }
                    Need::Exec if info.access.exec.is_empty() => {
                        Some("this node runs no programs".to_owned())
                    }
                    Need::FsRead | Need::FsWrite => off(info.filesystem.enabled, "files"),
                    Need::Exec => off(info.exec.enabled, "running programs"),
                    Need::Session => off(info.sessions.enabled, "detached sessions"),
                    Need::Power => off(!info.access.power.is_empty(), "a power source"),
                    Need::Media => off(!info.access.media.is_empty(), "a media source"),
                    Need::ForwardConnect => off(
                        info.forward.enabled && !info.forward.connect.is_empty(),
                        "forwarding to targets",
                    ),
                    Need::ForwardListen => off(
                        info.forward.enabled && !info.forward.listen.is_empty(),
                        "listening for forwards",
                    ),
                    Need::GuiView | Need::GuiInput | Need::GuiClipboard => {
                        let gui = &info.gui;
                        if !gui.enabled {
                            Some("the host has desktop access turned off".to_owned())
                        } else if !gui.available {
                            Some(format!(
                                "the desktop cannot be used now: {}",
                                gui.reason.as_deref().unwrap_or("no reason given")
                            ))
                        } else if need == Need::GuiInput && !gui.input {
                            Some("the host's desktop takes no input".to_owned())
                        } else if need == Need::GuiClipboard && !gui.clipboard {
                            Some("the host's desktop has no clipboard".to_owned())
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            }
        }
    }

    /// Every tool and whether it can run.
    #[must_use]
    pub fn available(&self) -> Vec<(&'static ToolSpec, Availability)> {
        REGISTRY
            .iter()
            .map(|spec| (spec, self.availability(spec)))
            .collect()
    }

    /// Calls the tool `name` with `arguments` (a JSON object).
    ///
    /// # Errors
    ///
    /// [`ToolError`] with code `unknown_tool` for a name not in [`REGISTRY`], `unavailable` for a tool this peer or host
    /// cannot run (with the reason), `bad_request` for arguments that do not fit the schema, else the tool's own.
    pub fn call(&self, name: &str, arguments: Value, cancel: &Cancel) -> Result<Output, ToolError> {
        let spec = REGISTRY
            .iter()
            .find(|spec| spec.name == name)
            .ok_or_else(|| {
                ToolError::new("unknown_tool", format!("there is no tool named `{name}`"))
            })?;
        if let Availability::Unavailable(reason) = self.availability(spec) {
            return Err(ToolError::new("unavailable", reason));
        }
        let arguments = match arguments {
            Value::Null => Value::Object(Map::new()),
            other => other,
        };
        (spec.run)(self, arguments, cancel).map_err(|mut error| {
            if error.code == "denied" {
                let needs: Vec<&str> = spec.needs.iter().map(|need| need.name()).collect();
                error.message = format!("{}; `{name}` needs: {}", error.message, needs.join(", "));
            }
            error
        })
    }

    pub(crate) fn link(&self) -> &dyn Open {
        &*self.link
    }

    pub(crate) fn transfer_wait(&self) -> Option<Duration> {
        self.settings.transfer_timeout
    }
}

/// Reads the arguments of a call.
pub(crate) fn parse<T: DeserializeOwned>(arguments: Value) -> Result<T, ToolError> {
    serde_json::from_value(arguments).map_err(|error| ToolError::bad(error.to_string()))
}

// ---- bytes in results -----------------------------------------------------------------------------------------------

const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding.
#[must_use]
pub fn base64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let n = (u32::from(group[0]) << 16)
            | (u32::from(*group.get(1).unwrap_or(&0)) << 8)
            | u32::from(*group.get(2).unwrap_or(&0));
        out.push(char::from(BASE64[(n >> 18) as usize & 63]));
        out.push(char::from(BASE64[(n >> 12) as usize & 63]));
        out.push(if group.len() > 1 {
            char::from(BASE64[(n >> 6) as usize & 63])
        } else {
            '='
        });
        out.push(if group.len() > 2 {
            char::from(BASE64[n as usize & 63])
        } else {
            '='
        });
    }
    out
}

/// Decodes standard base64 (padding optional, whitespace ignored).
///
/// # Errors
///
/// `None` for a character outside the alphabet or a length no input has.
#[must_use]
pub fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let (mut buffer, mut bits) = (0_u32, 0_u32);
    let mut padding = 0;
    for byte in text.bytes().filter(|byte| !byte.is_ascii_whitespace()) {
        if byte == b'=' {
            padding += 1;
            continue;
        }
        if padding > 0 {
            return None;
        }
        let value = BASE64.iter().position(|&c| c == byte)? as u32;
        buffer = (buffer << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    (bits < 6 && padding <= 2).then_some(out)
}

/// `bytes` as a JSON object: `{ "encoding": "utf8", "data": text }`, or `base64` when they are not text.
pub(crate) fn encoded(bytes: &[u8]) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(text) => json!({"encoding": "utf8", "data": text}),
        Err(_) => json!({"encoding": "base64", "data": base64_encode(bytes)}),
    }
}

/// The longest prefix of `bytes`, at most `max` long, that does not end inside a UTF-8 character when `bytes` is text.
pub(crate) fn text_cut(bytes: &[u8], max: usize) -> usize {
    let mut cut = max.min(bytes.len());
    if cut == bytes.len() {
        return cut;
    }
    // A UTF-8 character is at most four bytes: back up over continuation bytes, and the lead byte that owns them.
    let mut steps = 0;
    while cut > 0 && steps < 4 && (bytes[cut] & 0xC0) == 0x80 {
        cut -= 1;
        steps += 1;
    }
    if steps >= 4 || (bytes[cut] & 0xC0) == 0x80 {
        return max.min(bytes.len());
    }
    cut
}

/// The part of a program's output a call keeps: the first and last bytes within a limit, and how many lie between.
#[derive(Debug)]
pub(crate) struct Capture {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    head_cap: usize,
    tail_cap: usize,
    total: u64,
}

impl Capture {
    pub(crate) fn new(limit: usize) -> Self {
        let tail_cap = limit / 2;
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            head_cap: limit - tail_cap,
            tail_cap,
            total: 0,
        }
    }

    /// How many bytes have been pushed so far, kept or not.
    pub(crate) const fn pushed(&self) -> u64 {
        self.total
    }

    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.total += bytes.len() as u64;
        let mut rest = bytes;
        if self.head.len() < self.head_cap {
            let take = (self.head_cap - self.head.len()).min(rest.len());
            self.head.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
        }
        if rest.is_empty() || self.tail_cap == 0 {
            return;
        }
        if rest.len() >= self.tail_cap {
            self.tail.clear();
            self.tail.extend(&rest[rest.len() - self.tail_cap..]);
        } else {
            let over = (self.tail.len() + rest.len()).saturating_sub(self.tail_cap);
            self.tail.drain(..over);
            self.tail.extend(rest);
        }
    }

    /// The result: `{ encoding, data, tail?, total_bytes, omitted_bytes, truncated }`. A cut keeps whole characters of
    /// text; output that is not text is base64 and its cut is at bytes.
    pub(crate) fn finish(self) -> Value {
        let tail: Vec<u8> = self.tail.into_iter().collect();
        let kept = (self.head.len() + tail.len()) as u64;
        if kept == self.total {
            let mut all = self.head;
            all.extend_from_slice(&tail);
            let mut value = encoded(&all);
            value["total_bytes"] = json!(self.total);
            value["omitted_bytes"] = json!(0);
            value["truncated"] = json!(false);
            return value;
        }
        let head_text = match std::str::from_utf8(&self.head) {
            Ok(text) => Some(text),
            Err(error) if error.error_len().is_none() => {
                std::str::from_utf8(&self.head[..error.valid_up_to()]).ok()
            }
            Err(_) => None,
        };
        let skip = tail
            .iter()
            .take(3)
            .take_while(|byte| (**byte & 0xC0) == 0x80)
            .count();
        let tail_text = std::str::from_utf8(&tail[skip..]).ok();
        let (encoding, head_data, tail_data, shown) = match (head_text, tail_text) {
            (Some(head), Some(tail_text)) => (
                "utf8",
                head.to_owned(),
                tail_text.to_owned(),
                (head.len() + tail_text.len()) as u64,
            ),
            _ => (
                "base64",
                base64_encode(&self.head),
                base64_encode(&tail),
                kept,
            ),
        };
        json!({
            "encoding": encoding,
            "data": head_data,
            "tail": tail_data,
            "total_bytes": self.total,
            "omitted_bytes": self.total - shown,
            "truncated": true,
        })
    }
}

/// `{ "name": "...", ... }` as a JSON Schema object for a tool's arguments.
pub(crate) fn object(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_every_length() {
        for length in 0..40_usize {
            let bytes: Vec<u8> = (0..length).map(|i| (i * 37 + 11) as u8).collect();
            let text = base64_encode(&bytes);
            assert_eq!(base64_decode(&text).as_deref(), Some(bytes.as_slice()));
        }
        assert_eq!(base64_encode(b"Man"), "TWFu");
        assert_eq!(base64_encode(b"Ma"), "TWE=");
        assert_eq!(base64_decode("TWE").as_deref(), Some(&b"Ma"[..]));
        assert_eq!(base64_decode("T*Fu"), None);
    }

    #[test]
    fn a_capture_keeps_the_head_and_the_tail_and_counts_what_lies_between() {
        let mut capture = Capture::new(10);
        for chunk in (128..228_u8).collect::<Vec<_>>().chunks(7) {
            capture.push(chunk);
        }
        let value = capture.finish();
        assert_eq!(value["total_bytes"], 100);
        assert_eq!(value["truncated"], true);
        assert_eq!(value["encoding"], "base64");
        let head = base64_decode(value["data"].as_str().unwrap()).unwrap();
        let tail = base64_decode(value["tail"].as_str().unwrap()).unwrap();
        assert_eq!(head, (128..133).collect::<Vec<u8>>());
        assert_eq!(tail, (223..228).collect::<Vec<u8>>());
        assert_eq!(value["omitted_bytes"], 90);
    }

    #[test]
    fn a_text_capture_is_cut_between_characters() {
        let mut capture = Capture::new(8);
        capture.push("aé".repeat(10).as_bytes());
        let value = capture.finish();
        assert_eq!(value["encoding"], "utf8");
        let (head, tail) = (
            value["data"].as_str().unwrap(),
            value["tail"].as_str().unwrap(),
        );
        assert!(head.chars().all(|c| c == 'a' || c == 'é'));
        assert!(tail.chars().all(|c| c == 'a' || c == 'é'));
        let shown = (head.len() + tail.len()) as u64;
        assert_eq!(value["omitted_bytes"].as_u64().unwrap(), 30 - shown);
    }

    #[test]
    fn a_short_output_is_whole() {
        let mut capture = Capture::new(100);
        capture.push(b"hello ");
        capture.push(b"world");
        let value = capture.finish();
        assert_eq!(value["data"], "hello world");
        assert_eq!(value["truncated"], false);
        assert_eq!(value["omitted_bytes"], 0);
    }

    #[test]
    fn text_cut_never_splits_a_character() {
        let text = "aé€😀".as_bytes();
        for max in 0..=text.len() {
            let cut = text_cut(text, max);
            assert!(cut <= max);
            assert!(std::str::from_utf8(&text[..cut]).is_ok(), "max {max}");
        }
    }
}
