//! The `native` provider: this node's own pseudo-terminal and screen, offered through the same kinds as any provider.
//!
//! Nothing in the engines treats it differently. A terminal is `portable-pty` (`openpty` on Unix, ConPTY on Windows) in a
//! tree that is always ended; the desktop is the platform's backend where there is one, and `unavailable` where there is
//! none.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use bohio_api::guatiao::library::{Object, ProviderError};
use bohio_api::guatiao::{FromValue, Provider, Schema, Status, ToValue};
use bohio_api::model::{Delivery, EnvVar, SourceInfo, TerminalExit, TerminalOpen};
use bohio_api::{
    DesktopSession, DesktopSource, ExecSource, FilesSource, Terminal, TerminalSource, unavailable,
    vocab,
};
use bohio_proto::exec::Env;
use bohio_proto::shell::ShellKind;

use crate::config::{HostDeath, Span, WindowsCapture};
use crate::platform::{self, ProcessExit, PtySpec, Session, Signal, find_program, resolve_listed};
use bohio_x11::source::DesktopOf;

#[path = "native_exec.rs"]
mod exec_impl;

/// The id of the provider.
pub const ID: &str = "native";

/// `[source.native]`: how this node's own terminal and screen behave. Every key is optional.
#[derive(ToValue, FromValue, Schema, Debug, Clone, PartialEq, Eq)]
#[map(crate = ::bohio_api::guatiao)]
pub struct NativeSettings {
    /// Bytes of one read of a terminal's output. At least 1. Default `16384`.
    #[map(default = 16_384)]
    pub read_chunk: u64,
    /// Chunks of output a terminal holds ahead of the reader; its reader waits when they are full, so nothing buffers
    /// without bound. At least 1. Default `16`.
    #[map(default = 16)]
    pub queue_chunks: u64,
    /// Directory entries one read of a listing returns. At least 1. Default `256`.
    #[map(default = 256)]
    pub list_chunk: u64,
    /// How the Windows desktop captures: `auto` (DXGI, and GDI where DXGI cannot serve), `dxgi` or `gdi`. Default `auto`.
    #[map(default = String::from("auto"))]
    pub windows_capture: String,
    /// How long the Windows desktop waits for a display's first frame from DXGI before it captures that display with GDI
    /// instead (`250ms`, `0s` never waits). Default `250ms`.
    #[map(default = String::from("250ms"))]
    pub windows_frame_wait: String,
    /// How long the Windows desktop keeps its DXGI duplications after the last capture: each makes the compositor track
    /// the display's changes (`10s`, `0s` releases them after every capture). Default `10s`.
    #[map(default = String::from("10s"))]
    pub windows_capture_idle: String,
    /// How long the Windows desktop retries the clipboard while another program holds it open (`500ms`, `0s` tries once).
    /// Default `500ms`.
    #[map(default = String::from("500ms"))]
    pub windows_clipboard_wait: String,
    /// The most bytes of clipboard text the desktop reads or writes. At least 1. Default `1048576`.
    #[map(default = 1_048_576)]
    pub clipboard_max_bytes: u64,
    /// The X display the Linux desktop serves (`:0`). The `bohio` command fills it from `DISPLAY` when the file leaves it
    /// out; an embedding host sets it. Without one the node has no desktop on Linux.
    pub x11_display: Option<String>,
    /// Whether the session is a Wayland one: the command fills it with `WAYLAND_DISPLAY`'s value (or `wayland` for
    /// `XDG_SESSION_TYPE`). Its X display is XWayland's, which holds only X11 windows, so the desktop is refused unless
    /// `x11_in_wayland` is set.
    pub x11_wayland: Option<String>,
    /// Serve a Wayland session's X display all the same, its X11 windows only. Default `false`.
    #[map(default = false)]
    pub x11_in_wayland: bool,
    /// The login session logind is asked about (`XDG_SESSION_ID`, filled in by the command); absent, this process's own.
    pub x11_session: Option<String>,
    /// The system bus's address, when it is not the standard one. A host without logind (or with a bus that does not
    /// answer) has no lock detection: `info` says so and the lock state is unknown.
    pub x11_system_bus: Option<String>,
    /// How long logind may take to answer (`1s`). Not zero. Default `1s`.
    #[map(default = String::from("1s"))]
    pub x11_bus_wait: String,
    /// How long the keyboard mapping stays changed around a character typed through a spare key (`20ms`): programs that
    /// follow a mapping change need to see it. `0s` does not wait. Default `20ms`.
    #[map(default = String::from("20ms"))]
    pub x11_key_wait: String,
    /// How long a clipboard read waits for the program that owns the selection (`500ms`). Default `500ms`.
    #[map(default = String::from("500ms"))]
    pub x11_clipboard_wait: String,
    /// The selection that is the clipboard: `CLIPBOARD`, `PRIMARY` or any other name. Default `CLIPBOARD`.
    #[map(default = String::from("CLIPBOARD"))]
    pub x11_selection: String,
    /// The most clicks of one button event and wheel notches of one scroll event on X11; more is `bad_request`. At least
    /// 1. Default `1000`.
    #[map(default = 1_000)]
    pub x11_max_repeat: u64,
    /// Whether the Wayland desktop's portal remembers its approval: `none` (asked at every session), `node` (once for each
    /// run of the node) or `always` (a restore token kept in `wayland_restore_file`, so a portal that honours it asks once
    /// for good). Only a build with the `gui-wayland` feature has the backend. Default `node`.
    #[map(default = String::from("node"))]
    pub wayland_persist: String,
    /// Where the Wayland desktop keeps the portal's restore token under `wayland_persist = "always"` (`0600`). The
    /// `bohio` command fills it with `gui-wayland-restore` in the data directory when the file leaves it out.
    pub wayland_restore_file: Option<String>,
    /// How long a request waits for a monitor's first picture once the portal has started the session (`5s`; not `0s`).
    /// Default `5s`.
    #[map(default = String::from("5s"))]
    pub wayland_capture_wait: String,
    /// How long a request waits for the portal's dialog to be answered before it reports the dialog as pending (`10s`,
    /// `0s` never waits; the session goes on starting). Default `10s`.
    #[map(default = String::from("10s"))]
    pub wayland_start_wait: String,
    /// How long after a refused or failed start the portal is not asked again (`30s`, `0s` asks at every request). A peer
    /// that may ask `info` can make the portal's dialog appear, so this is how often at most. Default `30s`.
    #[map(default = String::from("30s"))]
    pub wayland_retry: String,
    /// How long the Wayland desktop keeps its portal session after the last request (`60s`, `0s` closes it after every request).
    /// The session keeps the compositor's screen-sharing indicator on and copies every frame; a portal that asks the person
    /// before sharing asks again when the session is reopened. Default `60s`.
    #[map(default = String::from("60s"))]
    pub wayland_idle: String,
    /// The most clicks of one button event and wheel notches of one scroll event on Wayland; more is `bad_request`. At
    /// least 1. Default `1000`.
    #[map(default = 1_000)]
    pub wayland_max_repeat: u64,
    /// The desktop a node run as a service account serves of its own (Linux): `none` (the session the command was started
    /// in), `x11` (the account's user units run a headless compositor and an X server on display `:UID`; capture and
    /// XTest input) or `wayland` (a compositor and the desktop portals; needs the `gui-wayland` feature). The runtime
    /// directory (`own_runtime_dir`) must be the account's own and mode 0700, or the desktop is `unavailable` naming it;
    /// an explicit `x11_display` still wins. Default `none`.
    #[map(default = String::from("none"))]
    pub own_session: String,
    /// The runtime directory of the own session, when it is not `/run/user/UID`.
    pub own_runtime_dir: Option<String>,
    /// Whether the node refuses the own desktop's X server unless the kernel says the node's own account runs it
    /// (`SO_PEERCRED` on the abstract socket). The socket's name is host-wide, so another account could bind it first and
    /// be sent what peers type and be asked for pictures. `false` serves whatever listens and is logged `non-default:`.
    /// Default `true`.
    #[map(default = true)]
    pub own_session_check_server: bool,
    /// Whether that directory must belong to the node's uid and be exactly mode 0700. `false` keeps the test that it is a
    /// directory and not a link and drops the other two, logged `non-default:`: another account that can enter the directory can
    /// reach the compositor's and X server's sockets, so the desktop. Default `true`.
    #[map(default = true)]
    pub own_runtime_dir_check: bool,
    /// How long the macOS desktop waits for ScreenCaptureKit to answer one capture (`10s`; not `0s`: it would never
    /// capture). Default `10s`.
    #[map(default = String::from("10s"))]
    pub macos_capture_wait: String,
    /// The most clicks of one button event and wheel notches of one scroll event on macOS; more is `bad_request`. At
    /// least 1. Default `1000`.
    #[map(default = 1_000)]
    pub macos_max_repeat: u64,
}

impl Default for NativeSettings {
    fn default() -> Self {
        Self {
            read_chunk: 16_384,
            queue_chunks: 16,
            list_chunk: 256,
            windows_capture: "auto".to_owned(),
            windows_frame_wait: "250ms".to_owned(),
            windows_capture_idle: "10s".to_owned(),
            windows_clipboard_wait: "500ms".to_owned(),
            clipboard_max_bytes: 1_048_576,
            x11_display: None,
            x11_wayland: None,
            x11_in_wayland: false,
            x11_session: None,
            x11_system_bus: None,
            x11_bus_wait: "1s".to_owned(),
            x11_key_wait: "20ms".to_owned(),
            x11_clipboard_wait: "500ms".to_owned(),
            x11_selection: "CLIPBOARD".to_owned(),
            x11_max_repeat: 1_000,
            wayland_persist: "node".to_owned(),
            wayland_restore_file: None,
            wayland_capture_wait: "5s".to_owned(),
            wayland_start_wait: "10s".to_owned(),
            wayland_retry: "30s".to_owned(),
            wayland_idle: "60s".to_owned(),
            wayland_max_repeat: 1_000,
            own_session: "none".to_owned(),
            own_runtime_dir: None,
            own_session_check_server: true,
            own_runtime_dir_check: true,
            macos_capture_wait: "10s".to_owned(),
            macos_max_repeat: 1_000,
        }
    }
}

impl NativeSettings {
    fn check(&self) -> Result<(), ProviderError> {
        let refuse = |message: &str| ProviderError::new(Status::GUATIAO_ERR_BAD_VALUE, message);
        if self.read_chunk == 0 {
            return Err(refuse(
                "read_chunk is 0: a read of no bytes never sees any output",
            ));
        }
        if self.queue_chunks == 0 {
            return Err(refuse("queue_chunks is 0: output has nowhere to wait"));
        }
        if self.list_chunk == 0 {
            return Err(refuse("list_chunk is 0: a listing never returns an entry"));
        }
        if self.clipboard_max_bytes == 0 {
            return Err(refuse("clipboard_max_bytes is 0: no clipboard text fits"));
        }
        if capture(&self.windows_capture).is_none() {
            return Err(refuse("windows_capture is auto, dxgi or gdi"));
        }
        for (key, text) in [
            ("windows_frame_wait", &self.windows_frame_wait),
            ("windows_capture_idle", &self.windows_capture_idle),
            ("windows_clipboard_wait", &self.windows_clipboard_wait),
            ("x11_key_wait", &self.x11_key_wait),
            ("x11_clipboard_wait", &self.x11_clipboard_wait),
            ("wayland_start_wait", &self.wayland_start_wait),
            ("wayland_retry", &self.wayland_retry),
            ("wayland_idle", &self.wayland_idle),
        ] {
            if Span::parse(text).is_none() {
                return Err(refuse(&format!("{key} is a time such as 250ms or 10s")));
            }
        }
        if Span::parse(&self.x11_bus_wait).is_none_or(|span| span.0.is_zero()) {
            return Err(refuse(
                "x11_bus_wait is a time such as 1s, and not zero: logind would never be asked",
            ));
        }
        if !["none", "wayland", "x11"].contains(&self.own_session.as_str()) {
            return Err(refuse("own_session is none, wayland or x11"));
        }
        if self.own_session != "none" && !self.own_runtime_dir_check {
            tracing::warn!(
                "non-default: [source.native] own_runtime_dir_check is false: the own session's runtime directory is not checked for its owner or mode 0700, so another account that can enter it can reach the compositor's and X server's sockets and so the desktop"
            );
        }
        if self.own_session != "none" && !cfg!(target_os = "linux") {
            return Err(refuse(
                "own_session is for a Linux node: a service account's own desktop",
            ));
        }
        if self.own_session != "none" && !self.own_session_check_server {
            tracing::warn!(
                "non-default: [source.native] own_session_check_server is false: the own desktop's X server is not checked to be run by the node's account, so another account that binds its socket first is sent what peers type"
            );
        }
        if !["none", "node", "always"].contains(&self.wayland_persist.as_str()) {
            return Err(refuse("wayland_persist is none, node or always"));
        }
        if Span::parse(&self.wayland_capture_wait).is_none_or(|span| span.0.is_zero()) {
            return Err(refuse(
                "wayland_capture_wait is a time such as 5s, and not zero: a capture would never be waited for",
            ));
        }
        if self.wayland_max_repeat == 0 {
            return Err(refuse(
                "wayland_max_repeat is 0: no click or notch is allowed",
            ));
        }
        if self.x11_max_repeat == 0 {
            return Err(refuse("x11_max_repeat is 0: no click or notch is allowed"));
        }
        if Span::parse(&self.macos_capture_wait).is_none_or(|span| span.0.is_zero()) {
            return Err(refuse(
                "macos_capture_wait is a time such as 10s, and not zero: a capture would never be waited for",
            ));
        }
        if self.macos_max_repeat == 0 {
            return Err(refuse(
                "macos_max_repeat is 0: no click or notch is allowed",
            ));
        }
        if self.x11_selection.is_empty() {
            return Err(refuse(
                "x11_selection is empty: name a selection such as CLIPBOARD",
            ));
        }
        Ok(())
    }

    /// The own session this node is set to serve, and the node's uid when its runtime directory is the account's, or the
    /// refusal naming the directory.
    #[cfg(target_os = "linux")]
    fn own(&self) -> Option<(crate::gui::OwnSession, Result<u32, String>)> {
        let kind = crate::gui::OwnSession::parse(&self.own_session)?;
        let checked = crate::gui::own_uid()
            .map_err(|error| {
                format!("own_session: cannot tell which account this node runs as: {error}")
            })
            .and_then(|uid| {
                let dir = self
                    .own_runtime_dir
                    .as_deref()
                    .filter(|dir| !dir.is_empty())
                    .map_or_else(|| crate::gui::own_runtime_dir(uid), PathBuf::from);
                crate::gui::check_runtime_dir(&dir, uid, self.own_runtime_dir_check)
                    .map(|()| uid)
                    .map_err(|reason| format!("own_session = \"{}\": {reason}", self.own_session))
            });
        Some((kind, checked))
    }

    #[cfg(target_os = "linux")]
    fn x11(&self) -> crate::gui::X11Settings {
        let mut settings = self.x11_plain();
        let Some((kind, checked)) = self.own() else {
            return settings;
        };
        // The own desktop is a compositor and a server no person sits at: not a login session, and no lock screen.
        settings.wayland = false;
        settings.headless = true;
        match checked {
            Err(reason) => {
                settings.display = None;
                settings.refused = Some(reason);
            }
            Ok(_) if kind == crate::gui::OwnSession::Wayland && !cfg!(feature = "gui-wayland") => {
                settings.refused = Some(
                    "own_session = \"wayland\" needs a build with the gui-wayland feature"
                        .to_owned(),
                );
            }
            Ok(uid) => {
                if settings.display.is_none() {
                    settings.display = Some(crate::gui::own_display(uid));
                    if self.own_session_check_server {
                        settings.connector = Some(Arc::new(crate::gui::OwnServer::new(uid)));
                    }
                }
                settings.hint = Some(format!(
                    "this node's own desktop is not up: as the account with uid {uid} look at `systemctl --user status \
                     bohio-desktop.target bohio-compositor.service bohio-xwayland.service`"
                ));
            }
        }
        settings
    }

    #[cfg(target_os = "linux")]
    fn x11_plain(&self) -> crate::gui::X11Settings {
        let span = |text: &str| Span::parse(text).map_or(Duration::ZERO, |span| span.0);
        let mut settings = crate::gui::X11Settings::default();
        settings.display = self
            .x11_display
            .clone()
            .filter(|display| !display.is_empty());
        settings.wayland = self
            .x11_wayland
            .as_ref()
            .is_some_and(|word| !word.is_empty());
        settings.in_wayland = self.x11_in_wayland;
        settings.session = self.x11_session.clone().filter(|id| !id.is_empty());
        settings.system_bus = self.x11_system_bus.clone().filter(|bus| !bus.is_empty());
        settings.bus_wait = span(&self.x11_bus_wait);
        settings.key_wait = span(&self.x11_key_wait);
        settings.clipboard_wait = span(&self.x11_clipboard_wait);
        settings.clipboard_max_bytes =
            usize::try_from(self.clipboard_max_bytes).unwrap_or(usize::MAX);
        settings.selection = self.x11_selection.clone();
        settings.max_repeat = u32::try_from(self.x11_max_repeat).unwrap_or(u32::MAX);
        settings
    }

    /// Whether this node's session is a Wayland one and the file does not ask for its X11 windows only.
    #[cfg(all(target_os = "linux", feature = "gui-wayland"))]
    fn serves_wayland(&self) -> bool {
        if let Some((kind, checked)) = self.own() {
            return kind == crate::gui::OwnSession::Wayland && checked.is_ok();
        }
        !self.x11_in_wayland
            && self
                .x11_wayland
                .as_ref()
                .is_some_and(|word| !word.is_empty())
    }

    #[cfg(all(target_os = "linux", feature = "gui-wayland"))]
    fn wayland(&self) -> crate::gui::WaylandSettings {
        let span = |text: &str| Span::parse(text).map_or(Duration::ZERO, |span| span.0);
        crate::gui::WaylandSettings {
            persist: crate::gui::Persist::parse(&self.wayland_persist).unwrap_or_default(),
            restore_file: self
                .wayland_restore_file
                .clone()
                .filter(|path| !path.is_empty())
                .map(std::path::PathBuf::from),
            capture_wait: span(&self.wayland_capture_wait),
            start_wait: span(&self.wayland_start_wait),
            retry: span(&self.wayland_retry),
            idle: span(&self.wayland_idle),
            session: self.x11_session.clone().filter(|id| !id.is_empty()),
            system_bus: self.x11_system_bus.clone().filter(|bus| !bus.is_empty()),
            bus_wait: span(&self.x11_bus_wait),
            headless: self.own_session != "none",
            max_repeat: u32::try_from(self.wayland_max_repeat).unwrap_or(u32::MAX),
        }
    }

    #[cfg(target_os = "macos")]
    fn macos(&self) -> crate::gui::MacosSettings {
        crate::gui::MacosSettings {
            capture_wait: Span::parse(&self.macos_capture_wait)
                .map_or(Duration::ZERO, |span| span.0),
            clipboard_max_bytes: usize::try_from(self.clipboard_max_bytes).unwrap_or(usize::MAX),
            max_repeat: u32::try_from(self.macos_max_repeat).unwrap_or(u32::MAX),
        }
    }

    #[cfg(windows)]
    fn windows(&self) -> crate::gui::WindowsSettings {
        let span = |text: &str| Span::parse(text).map_or(Duration::ZERO, |span| span.0);
        crate::gui::WindowsSettings {
            capture: capture(&self.windows_capture).unwrap_or(WindowsCapture::Auto),
            frame_wait: span(&self.windows_frame_wait),
            capture_idle: span(&self.windows_capture_idle),
            clipboard_wait: span(&self.windows_clipboard_wait),
            clipboard_max_bytes: usize::try_from(self.clipboard_max_bytes).unwrap_or(usize::MAX),
        }
    }
}

fn capture(text: &str) -> Option<WindowsCapture> {
    match text {
        "auto" => Some(WindowsCapture::Auto),
        "dxgi" => Some(WindowsCapture::Dxgi),
        "gdi" => Some(WindowsCapture::Gdi),
        _ => None,
    }
}

/// This node's own terminal and desktop.
#[derive(Default, Provider)]
#[provider(
    TerminalSource,
    DesktopSource,
    ExecSource,
    FilesSource,
    id = "native",
    name = "native",
    crate = ::bohio_api::guatiao,
    config = NativeSettings
)]
pub struct Native {
    settings: NativeSettings,
    desktop: Mutex<Option<Arc<DesktopOf>>>,
}

impl TryFrom<NativeSettings> for Native {
    type Error = ProviderError;

    fn try_from(settings: NativeSettings) -> Result<Self, Self::Error> {
        settings.check()?;
        Ok(Self {
            settings,
            desktop: Mutex::new(None),
        })
    }
}

impl Native {
    /// Entries one read of a listing returns.
    pub(super) fn list_chunk(&self) -> u64 {
        self.settings.list_chunk
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn refuse(status: Status, message: &str) -> ProviderError {
    ProviderError::new(status, message)
}

impl TerminalSource for Native {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        Ok(SourceInfo::new(
            ID,
            &[
                vocab::terminal::READ,
                vocab::terminal::WRITE,
                vocab::terminal::PROGRAMS,
                vocab::terminal::RESIZE,
                vocab::terminal::SIGNALS,
                vocab::terminal::EXIT_STATUS,
            ],
            &[vocab::terminal::BREAK],
        ))
    }

    fn open(&self, request: TerminalOpen) -> Result<Object<dyn Terminal>, ProviderError> {
        // A console request names no program: the node's console is its default shell.
        let default = bohio_api::ProgramSpec {
            kind: Some(if cfg!(windows) { "cmd" } else { "sh" }.to_owned()),
            ..bohio_api::ProgramSpec::default()
        };
        let program = request.program.as_ref().unwrap_or(&default);
        let path = request
            .path
            .as_deref()
            .map(OsString::from)
            .unwrap_or_default();
        let (found, args) = resolve(program, &path)?;
        if let Some(cwd) = request.cwd.as_deref()
            && !Path::new(cwd).is_dir()
        {
            return Err(refuse(
                Status::GUATIAO_ERR_NOT_FOUND,
                "the working directory does not exist",
            ));
        }
        let spec = PtySpec {
            program: found,
            args,
            cwd: request.cwd.as_deref().map(PathBuf::from),
            env: env_of(&request),
            cols: request.cols,
            rows: request.rows,
            on_host_death: match request.on_host_death.as_deref() {
                Some("term") => HostDeath::Term,
                Some("leave") => HostDeath::Leave,
                _ => HostDeath::Kill,
            },
        };
        let terminal = platform::spawn_terminal(spec).map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => {
                refuse(Status::GUATIAO_ERR_NOT_FOUND, "the program was not found")
            }
            std::io::ErrorKind::PermissionDenied => refuse(
                Status::GUATIAO_ERR_BAD_VALUE,
                "the operating system refused to run the program",
            ),
            _ => refuse(
                Status::GUATIAO_ERR_INTERNAL,
                "the shell could not be started",
            ),
        })?;
        Ok(Pty::start(
            terminal,
            usize::try_from(self.settings.read_chunk).unwrap_or(usize::MAX),
            usize::try_from(self.settings.queue_chunks).unwrap_or(usize::MAX),
        )
        .into_object())
    }
}

fn env_of(request: &TerminalOpen) -> Env {
    let mut env = Env {
        clear: request.clear_env,
        ..Env::default()
    };
    for EnvVar { name, value } in &request.env {
        match value {
            Some(value) => {
                env.set.insert(name.clone(), value.clone());
            }
            None => env.remove.push(name.clone()),
        }
    }
    env
}

/// The program and arguments a request names, found on the host as the grant says.
fn resolve(
    program: &bohio_api::ProgramSpec,
    path: &std::ffi::OsStr,
) -> Result<(OsString, Vec<OsString>), ProviderError> {
    if let Some(kind) = &program.kind {
        let name = ShellKind::KNOWN
            .into_iter()
            .map(ShellKind::program)
            .find(|known| known == kind)
            .ok_or_else(|| {
                refuse(
                    Status::GUATIAO_ERR_BAD_VALUE,
                    "this host does not know that kind of shell",
                )
            })?;
        let found = find_program(name, path)
            .ok_or_else(|| refuse(Status::GUATIAO_ERR_NOT_FOUND, "this host has no such shell"))?;
        return Ok((found.into_os_string(), Vec::new()));
    }
    let Some((first, rest)) = program.argv.split_first() else {
        return Err(refuse(
            Status::GUATIAO_ERR_BAD_VALUE,
            "the program is empty",
        ));
    };
    // Under a program list the program is the one the host finds at the listed name on its own `PATH`.
    let named = Path::new(first);
    let located = if program.listed {
        resolve_listed(first, path)
    } else if named.components().count() > 1 {
        named.is_file().then(|| named.to_owned())
    } else {
        find_program(first, path)
    }
    .ok_or_else(|| refuse(Status::GUATIAO_ERR_NOT_FOUND, "the program was not found"))?;
    // A batch file is read by `cmd`, which takes its arguments as commands: refused as `exec` refuses it.
    if cfg!(windows) && crate::platform::is_batch(&located) {
        return Err(refuse(
            Status::GUATIAO_ERR_BAD_VALUE,
            "a batch file is not started directly: its arguments would be read by cmd; run `cmd /C` on it",
        ));
    }
    Ok((
        located.into_os_string(),
        rest.iter().map(OsString::from).collect(),
    ))
}

/// One program on a pseudo-terminal. Its output is read on a thread of its own into a bounded queue, so `read` can time
/// out: a blocking read of the terminal is not reliably interrupted by ending the program.
struct Pty {
    session: Arc<Session>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    chunks: Mutex<Receiver<Vec<u8>>>,
    /// What the caller's buffer had no room for.
    pending: Mutex<Vec<u8>>,
    ended: AtomicBool,
}

impl Pty {
    fn start(terminal: platform::Terminal, chunk: usize, queued: usize) -> Self {
        let platform::Terminal {
            session,
            reader,
            writer,
        } = terminal;
        let (sender, chunks) = sync_channel::<Vec<u8>>(queued);
        let answering = Arc::clone(&writer);
        let _ = thread::Builder::new()
            .name("bohio-terminal-reader".to_owned())
            .spawn(move || pump(reader, &sender, &answering, chunk));
        Self {
            session,
            writer,
            chunks: Mutex::new(chunks),
            pending: Mutex::new(Vec::new()),
            ended: AtomicBool::new(false),
        }
    }
}

/// Copies the terminal's output into the queue until it ends, answering the cursor queries ConPTY asks on the way.
fn pump(
    mut reader: Box<dyn Read + Send>,
    sender: &SyncSender<Vec<u8>>,
    writer: &Arc<Mutex<Box<dyn Write + Send>>>,
    chunk: usize,
) {
    let mut buffer = vec![0_u8; chunk.max(1)];
    #[cfg(windows)]
    let mut queries = platform::CursorQueries::default();
    #[cfg(windows)]
    let asks = answer_queries(writer);
    #[cfg(not(windows))]
    let _ = writer;
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                #[cfg(windows)]
                let output = {
                    let (output, asked) = queries.filter(&buffer[..count]);
                    for _ in 0..asked {
                        let _ = asks.send(());
                    }
                    output
                };
                #[cfg(not(windows))]
                let output = buffer[..count].to_vec();
                if output.is_empty() {
                    continue;
                }
                if sender.send(output).is_err() {
                    return;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    #[cfg(windows)]
    {
        let held = queries.finish();
        if !held.is_empty() {
            let _ = sender.send(held);
        }
    }
}

/// Answers the terminal's cursor position queries on a thread of their own, so the reader never waits on a write that
/// holds the writer.
#[cfg(windows)]
fn answer_queries(writer: &Arc<Mutex<Box<dyn Write + Send>>>) -> std::sync::mpsc::Sender<()> {
    let (asks, asked) = std::sync::mpsc::channel::<()>();
    let writer = Arc::clone(writer);
    let _ = thread::Builder::new()
        .name("bohio-terminal-answers".to_owned())
        .spawn(move || {
            for () in asked {
                let mut writer = lock(&writer);
                let _ = writer
                    .write_all(platform::CURSOR_REPORT)
                    .and_then(|()| writer.flush());
            }
        });
    asks
}

fn exit_of(exit: ProcessExit) -> TerminalExit {
    TerminalExit {
        code: exit.code.map(i64::from),
        signal: exit.signal.map(i64::from),
    }
}

impl Terminal for Pty {
    fn read(&self, dst: &mut [u8], wait_ms: u64) -> Result<i64, ProviderError> {
        let take = |from: &mut Vec<u8>, dst: &mut [u8]| {
            let count = dst.len().min(from.len());
            dst[..count].copy_from_slice(&from[..count]);
            from.drain(..count);
            count
        };
        {
            let mut pending = lock(&self.pending);
            if !pending.is_empty() {
                let count = take(&mut pending, dst);
                return Ok(i64::try_from(count).unwrap_or(i64::MAX));
            }
        }
        if self.ended.load(Ordering::Acquire) {
            return Ok(-1);
        }
        let received = lock(&self.chunks).recv_timeout(Duration::from_millis(wait_ms));
        match received {
            Ok(mut chunk) => {
                let count = take(&mut chunk, dst);
                if !chunk.is_empty() {
                    lock(&self.pending).extend_from_slice(&chunk);
                }
                Ok(i64::try_from(count).unwrap_or(i64::MAX))
            }
            Err(RecvTimeoutError::Timeout) => Ok(0),
            Err(RecvTimeoutError::Disconnected) => {
                self.ended.store(true, Ordering::Release);
                Ok(-1)
            }
        }
    }

    fn write(&self, data: &[u8]) -> Result<u64, ProviderError> {
        let mut writer = lock(&self.writer);
        // A failed write means the program is gone; `exit` says so.
        match writer.write_all(data).and_then(|()| writer.flush()) {
            Ok(()) => Ok(data.len() as u64),
            Err(_) => Ok(0),
        }
    }

    fn kill(&self) -> Result<TerminalExit, ProviderError> {
        let ended = self.session.kill().unwrap_or(ProcessExit {
            code: None,
            signal: None,
        });
        self.session.close_terminal();
        Ok(exit_of(ended))
    }

    fn exit(&self) -> Result<Option<TerminalExit>, ProviderError> {
        match self.session.try_wait() {
            Ok(exit) => Ok(exit.map(exit_of)),
            Err(_) => Err(unavailable("the program cannot be looked at")),
        }
    }

    fn pid(&self) -> Result<Option<u64>, ProviderError> {
        Ok(Some(u64::from(self.session.id())))
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<(), ProviderError> {
        self.session
            .resize(cols, rows)
            .map_err(|_| unavailable("the terminal is closed"))
    }

    fn signal(&self, name: &str) -> Result<Delivery, ProviderError> {
        let signal = Signal::ALL
            .into_iter()
            .find(|signal| signal.name() == name)
            .ok_or_else(|| refuse(Status::GUATIAO_ERR_BAD_VALUE, "not a signal"))?;
        let running = matches!(self.session.try_wait(), Ok(None));
        let delivery = self.session.signal(signal, &self.writer);
        Ok(Delivery {
            delivered: delivery.delivered,
            ended: delivery.ended && running,
            message: delivery.message.map(str::to_owned),
        })
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // Nothing outlives a terminal: its tree ends with the last holder.
        let _ = self.session.kill();
        self.session.close_terminal();
    }
}

impl DesktopSource for Native {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        match self.platform_desktop() {
            Some(desktop) => desktop.describe(),
            None => Ok(SourceInfo::new(
                ID,
                &[],
                &[
                    vocab::desktop::CAPTURE,
                    vocab::desktop::INPUT,
                    vocab::desktop::CLIPBOARD,
                    vocab::desktop::LOCK_DETECTION,
                    vocab::desktop::MULTI_DISPLAY,
                ],
            )
            .unavailable("this node has no desktop backend for this operating system")),
        }
    }

    fn open(&self) -> Result<Object<dyn DesktopSession>, ProviderError> {
        match self.platform_desktop() {
            Some(desktop) => desktop.open(),
            None => Err(unavailable(
                "this node has no desktop backend for this operating system",
            )),
        }
    }
}

impl Native {
    /// The platform's desktop, made once: its capture state outlives one stream.
    #[cfg(windows)]
    fn platform_desktop(&self) -> Option<Arc<DesktopOf>> {
        let mut made = lock(&self.desktop);
        Some(Arc::clone(made.get_or_insert_with(|| {
            Arc::new(DesktopOf::new(Arc::new(crate::gui::WindowsDesktop::new(
                self.settings.windows(),
            ))))
        })))
    }

    /// The platform's desktop, made once: its connection outlives one stream.
    #[cfg(target_os = "linux")]
    fn platform_desktop(&self) -> Option<Arc<DesktopOf>> {
        let mut made = lock(&self.desktop);
        Some(Arc::clone(made.get_or_insert_with(|| {
            #[cfg(feature = "gui-wayland")]
            if self.settings.serves_wayland() {
                return Arc::new(DesktopOf::new(Arc::new(crate::gui::WaylandDesktop::new(
                    self.settings.wayland(),
                ))));
            }
            Arc::new(DesktopOf::new(Arc::new(crate::gui::X11Desktop::new(
                self.settings.x11(),
            ))))
        })))
    }

    /// The platform's desktop, made once: its event source outlives one stream.
    #[cfg(target_os = "macos")]
    fn platform_desktop(&self) -> Option<Arc<DesktopOf>> {
        let mut made = lock(&self.desktop);
        Some(Arc::clone(made.get_or_insert_with(|| {
            Arc::new(DesktopOf::new(Arc::new(crate::gui::MacosDesktop::new(
                self.settings.macos(),
            ))))
        })))
    }

    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    fn platform_desktop(&self) -> Option<Arc<DesktopOf>> {
        let _ = &self.desktop;
        None
    }
}

bohio_api::guatiao::local_providers!(
    id = "bohio-native",
    version = env!("CARGO_PKG_VERSION"),
    providers = [Native]
);

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::NativeSettings;

    fn own_desktop(check: bool) -> bool {
        let dir = tempfile::tempdir().expect("a directory");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("its mode");
        let settings = NativeSettings {
            own_session: "x11".to_owned(),
            own_runtime_dir: Some(dir.path().display().to_string()),
            own_session_check_server: check,
            ..NativeSettings::default()
        };
        settings.x11().connector.is_some()
    }

    #[test]
    fn the_own_desktop_is_reached_through_the_check_of_who_runs_it_unless_the_setting_is_off() {
        assert!(own_desktop(true));
        assert!(!own_desktop(false));
    }
}
