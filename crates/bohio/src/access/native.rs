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
use bohio_api::{DesktopSession, DesktopSource, Terminal, TerminalSource, unavailable, vocab};
use bohio_proto::exec::Env;
use bohio_proto::shell::ShellKind;

use super::desktop::DesktopOf;
use crate::config::{HostDeath, Span, WindowsCapture};
use crate::platform::{self, ProcessExit, PtySpec, Session, Signal, find_program, resolve_listed};

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
    /// The most bytes of clipboard text the Windows desktop reads or writes. At least 1. Default `1048576`.
    #[map(default = 1_048_576)]
    pub clipboard_max_bytes: u64,
}

impl Default for NativeSettings {
    fn default() -> Self {
        Self {
            read_chunk: 16_384,
            queue_chunks: 16,
            windows_capture: "auto".to_owned(),
            windows_frame_wait: "250ms".to_owned(),
            windows_capture_idle: "10s".to_owned(),
            windows_clipboard_wait: "500ms".to_owned(),
            clipboard_max_bytes: 1_048_576,
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
        ] {
            if Span::parse(text).is_none() {
                return Err(refuse(&format!("{key} is a time such as 250ms or 10s")));
            }
        }
        Ok(())
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

    #[cfg(not(windows))]
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
