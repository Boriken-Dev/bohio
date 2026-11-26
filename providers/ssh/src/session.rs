// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! One session channel running a program: its output on queues the host reads with a wait, its input, and how it ended.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use bohio_api::guatiao::Status;
use bohio_api::guatiao::library::ProviderError;
use bohio_api::model::{Delivery, TerminalExit};
use bohio_api::unavailable;
use russh::client::Msg;
use russh::{ChannelMsg, ChannelReadHalf, ChannelWriteHalf, Sig};
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::mpsc::{self, Receiver, Sender};

use crate::command::signal_number;
use crate::link::{Link, Slot, lock, refuse, within};

/// How the program ended, shared between the task that reads the channel and the callers.
#[derive(Default)]
struct Remote {
    state: Mutex<Exit>,
    changed: Condvar,
}

#[derive(Default, Clone, Copy)]
struct Exit {
    done: bool,
    code: Option<i64>,
    signal: Option<i64>,
}

impl Remote {
    fn finish(&self, code: Option<i64>, signal: Option<i64>) {
        let mut state = lock(&self.state);
        if !state.done {
            *state = Exit {
                done: true,
                code,
                signal,
            };
        }
        self.changed.notify_all();
    }

    fn snapshot(&self) -> Option<TerminalExit> {
        let state = *lock(&self.state);
        state.done.then_some(TerminalExit {
            code: state.code,
            signal: state.signal,
        })
    }

    fn wait(&self, limit: Duration) -> Option<TerminalExit> {
        let deadline = Instant::now() + limit;
        let mut state = lock(&self.state);
        while !state.done {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        let state = *state;
        state.done.then_some(TerminalExit {
            code: state.code,
            signal: state.signal,
        })
    }
}

/// One output of the program, read in chunks with a wait.
struct Stream {
    chunks: Receiver<Vec<u8>>,
    pending: Vec<u8>,
    taken: usize,
    ended: bool,
}

impl Stream {
    fn new(chunks: Receiver<Vec<u8>>) -> Mutex<Self> {
        Mutex::new(Self {
            chunks,
            pending: Vec::new(),
            taken: 0,
            ended: false,
        })
    }

    fn give(&mut self, dst: &mut [u8]) -> i64 {
        let count = dst.len().min(self.pending.len() - self.taken);
        dst[..count].copy_from_slice(&self.pending[self.taken..self.taken + count]);
        self.taken += count;
        if self.taken == self.pending.len() {
            self.pending.clear();
            self.taken = 0;
        }
        i64::try_from(count).unwrap_or(i64::MAX)
    }
}

/// How a channel is started.
pub(crate) struct Open {
    /// A pseudo-terminal of `(TERM, columns, rows)`; its output has no separate standard error.
    pub pty: Option<(String, u32, u32)>,
    /// The line to run; absent, the host's login shell.
    pub command: Option<String>,
    /// Whether input is open; otherwise it ends at once.
    pub stdin: bool,
}

/// A program running on the host.
pub(crate) struct Session {
    link: Arc<Link>,
    backend: Backend,
    out: Mutex<Stream>,
    err: Mutex<Stream>,
    remote: Arc<Remote>,
    killed: AtomicBool,
    /// Lets go of the channel's slot without waiting for the host to answer a close.
    stop: Arc<tokio::sync::Notify>,
}

impl Session {
    pub(crate) async fn start(link: &Arc<Link>, open: Open) -> Result<Self, ProviderError> {
        if link.settings.openssh.is_some() {
            return Self::start_child(link, open).await;
        }
        let Slot { channel, permit } = link.channel().await?;
        let wait = link.settings.operation_timeout;
        let refused = |what: &str| {
            refuse(
                Status::GUATIAO_ERR_INTERNAL,
                format!("{}: the host refused {what}", link.target()),
            )
        };
        // The host answers each request in order; `true` marks the X forwarding request, which may fail without ending
        // the program (the host may not allow it).
        let mut replies: Vec<bool> = Vec::new();
        let sent = async {
            if let Some((term, cols, rows)) = &open.pty {
                channel
                    .request_pty(true, term.as_str(), *cols, *rows, 0, 0, &[])
                    .await?;
                replies.push(false);
            }
            if let Some(forward) = &link.forward {
                channel
                    .request_x11(
                        true,
                        false,
                        crate::forward::PROTOCOL,
                        forward.cookie_hex(),
                        0,
                    )
                    .await?;
                replies.push(true);
            }
            match &open.command {
                Some(command) => channel.exec(true, command.as_str()).await?,
                None => channel.request_shell(true).await?,
            }
            replies.push(false);
            Ok::<(), russh::Error>(())
        };
        match tokio::time::timeout(wait, sent).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                return Err(refuse(
                    Status::GUATIAO_ERR_GONE,
                    format!("{}: {error}", link.target()),
                ));
            }
            Err(_) => return Err(unavailable("the host did not answer in time")),
        }
        let (mut read, write) = channel.split();
        let queue = link.settings.queue_chunks;
        let (out_tx, out_rx) = mpsc::channel(queue);
        let (err_tx, err_rx) = mpsc::channel(queue);
        let merge = open.pty.is_some();

        // Output may follow the answers at once and is kept.
        let answered = tokio::time::timeout(wait, async {
            let mut got = 0;
            while got < replies.len() {
                match read.wait().await {
                    Some(ChannelMsg::Success) => got += 1,
                    Some(ChannelMsg::Failure) if replies[got] => {
                        tracing::warn!(
                            "ssh source {}: the host refused X forwarding (it needs X11Forwarding yes and an xauth); the program runs without it",
                            link.target()
                        );
                        got += 1;
                    }
                    Some(ChannelMsg::Data { data }) => {
                        let _ = out_tx.send(data.to_vec()).await;
                    }
                    Some(ChannelMsg::ExtendedData { data, .. }) => {
                        let tx = if merge { &out_tx } else { &err_tx };
                        let _ = tx.send(data.to_vec()).await;
                    }
                    Some(ChannelMsg::Failure | ChannelMsg::OpenFailure(_)) | None => {
                        return Err(());
                    }
                    Some(_) => {}
                }
            }
            Ok(())
        })
        .await;
        match answered {
            Ok(Ok(())) => {}
            Ok(Err(())) => {
                return Err(refused(if open.pty.is_some() {
                    "the terminal or the command"
                } else {
                    "to run the command"
                }));
            }
            Err(_) => return Err(unavailable("the host did not answer in time")),
        }
        if !open.stdin {
            let _ = write.eof().await;
        }
        let remote = Arc::new(Remote::default());
        let stop = Arc::new(tokio::sync::Notify::new());
        tokio::spawn(pump(
            read,
            Pump {
                stop: Arc::clone(&stop),
                out: out_tx,
                err: err_tx,
                merge,
                remote: Arc::clone(&remote),
                grace: link.settings.exit_grace,
                _permit: permit,
            },
        ));
        Ok(Self {
            link: Arc::clone(link),
            backend: Backend::Channel(Arc::new(write)),
            out: Stream::new(out_rx),
            err: Stream::new(err_rx),
            remote,
            killed: AtomicBool::new(false),
            stop,
        })
    }

    /// Reads `stream` (1 standard output, 2 standard error): the bytes, `0` when none came in time, `-1` at the end.
    pub(crate) fn read(
        &self,
        stream: u8,
        dst: &mut [u8],
        wait_ms: u64,
    ) -> Result<i64, ProviderError> {
        let which = match stream {
            1 => &self.out,
            2 => &self.err,
            _ => {
                return Err(refuse(
                    Status::GUATIAO_ERR_BAD_VALUE,
                    "a stream is 1 (standard output) or 2 (standard error)",
                ));
            }
        };
        let mut held = lock(which);
        if held.taken < held.pending.len() {
            return Ok(held.give(dst));
        }
        if held.ended {
            return Ok(-1);
        }
        let wait = Duration::from_millis(wait_ms);
        let received = self
            .link
            .rt
            .block_on(async { tokio::time::timeout(wait, held.chunks.recv()).await });
        match received {
            Ok(Some(chunk)) => {
                held.pending = chunk;
                held.taken = 0;
                Ok(held.give(dst))
            }
            Ok(None) => {
                held.ended = true;
                Ok(-1)
            }
            Err(_) => Ok(0),
        }
    }

    pub(crate) fn write(&self, data: &[u8]) -> Result<u64, ProviderError> {
        if self.remote.snapshot().is_some() {
            return Err(unavailable("the program has ended"));
        }
        match &self.backend {
            Backend::Channel(write) => within(
                &self.link,
                self.link.settings.operation_timeout,
                write.data_bytes(data.to_vec()),
            )?
            .map(|()| data.len() as u64)
            .map_err(|error| unavailable(&format!("{}: {error}", self.link.target()))),
            Backend::Child(child) => {
                within(&self.link, self.link.settings.operation_timeout, async {
                    let mut stdin = child.stdin.lock().await;
                    let Some(stdin) = stdin.as_mut() else {
                        return Err(unavailable("the input is closed"));
                    };
                    tokio::io::AsyncWriteExt::write_all(stdin, data)
                        .await
                        .map(|()| data.len() as u64)
                        .map_err(|error| unavailable(&format!("the ssh program: {error}")))
                })?
            }
        }
    }

    pub(crate) fn close_input(&self) -> Result<(), ProviderError> {
        match &self.backend {
            Backend::Channel(write) => within(
                &self.link,
                self.link.settings.operation_timeout,
                write.eof(),
            )?
            .map_err(|error| unavailable(&format!("{}: {error}", self.link.target()))),
            Backend::Child(child) => {
                // Dropping the pipe ends the program's input.
                within(&self.link, self.link.settings.operation_timeout, async {
                    child.stdin.lock().await.take();
                })
            }
        }
    }

    pub(crate) fn wait(&self, wait_ms: u64) -> Option<TerminalExit> {
        self.remote.wait(Duration::from_millis(wait_ms))
    }

    pub(crate) fn exit(&self) -> Option<TerminalExit> {
        self.remote.snapshot()
    }

    /// Ends the program: the host is asked to kill it and the channel is closed, and the program is over from here
    /// even when the host never answers.
    pub(crate) fn kill(&self) -> TerminalExit {
        if !self.killed.swap(true, Ordering::AcqRel) {
            match &self.backend {
                Backend::Channel(write) => {
                    let write = Arc::clone(write);
                    let _ = within(
                        &self.link,
                        self.link.settings.operation_timeout,
                        async move {
                            let _ = write.signal(Sig::KILL).await;
                            let _ = write.close().await;
                        },
                    );
                }
                // The ssh program ends; the host's program is left to its own hangup.
                Backend::Child(child) => child.kill.notify_one(),
            }
        }
        self.remote.finish(None, signal_number("KILL"));
        self.stop.notify_one();
        self.remote.snapshot().unwrap_or_default()
    }

    pub(crate) fn resize(&self, cols: u16, rows: u16) -> Result<(), ProviderError> {
        let Backend::Channel(write) = &self.backend else {
            return Err(refuse(
                Status::GUATIAO_ERR_UNSUPPORTED,
                "the system's ssh client cannot resize a terminal",
            ));
        };
        within(
            &self.link,
            self.link.settings.operation_timeout,
            write.window_change(u32::from(cols), u32::from(rows), 0, 0),
        )?
        .map_err(|error| unavailable(&format!("{}: {error}", self.link.target())))
    }

    /// Sends the SSH signal `sig`; the host decides whether it reaches the program.
    pub(crate) fn signal(&self, sig: Sig, ends: bool) -> Result<Delivery, ProviderError> {
        let running = self.remote.snapshot().is_none();
        let Backend::Channel(write) = &self.backend else {
            return Err(refuse(
                Status::GUATIAO_ERR_UNSUPPORTED,
                "the system's ssh client cannot send a signal to the host's program",
            ));
        };
        within(
            &self.link,
            self.link.settings.operation_timeout,
            write.signal(sig),
        )?
        .map_err(|error| unavailable(&format!("{}: {error}", self.link.target())))?;
        Ok(Delivery {
            delivered: true,
            ended: ends && running,
            message: Some(
                "sent as an SSH signal request; the host decides whether it reaches the program, and a request that arrives before the program has started is lost"
                    .to_owned(),
            ),
        })
    }

    /// Writes one control byte to a terminal: what the line discipline turns into a signal for the foreground group.
    pub(crate) fn control(&self, byte: u8) -> Result<(), ProviderError> {
        self.write(&[byte]).map(drop)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.killed.swap(true, Ordering::AcqRel) && self.remote.snapshot().is_none() {
            self.stop.notify_one();
            match &self.backend {
                Backend::Channel(write) => {
                    let write = Arc::clone(write);
                    self.link.rt.handle().spawn(async move {
                        let _ = write.signal(Sig::KILL).await;
                        let _ = write.close().await;
                    });
                }
                Backend::Child(child) => child.kill.notify_one(),
            }
        }
    }
}

/// How a program is reached: a channel of the connection, or a process of the system's ssh client.
enum Backend {
    Channel(Arc<ChannelWriteHalf<Msg>>),
    Child(ChildCtl),
}

/// The ends of an ssh process this side holds.
struct ChildCtl {
    stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
    kill: Arc<tokio::sync::Notify>,
}

impl Session {
    /// Runs the program as a process of the system's ssh client.
    async fn start_child(link: &Arc<Link>, open: Open) -> Result<Self, ProviderError> {
        use tokio::io::AsyncReadExt as _;
        let permit = link.permit().await?;
        let after: Vec<&str> = open.command.as_deref().into_iter().collect();
        let mut command = crate::openssh::command(
            link,
            &[if open.pty.is_some() { "-tt" } else { "-T" }],
            &after,
        )?;
        if let Some((term, _, _)) = &open.pty {
            command.env("TERM", term);
        }
        let mut child = command.spawn().map_err(|error| {
            refuse(
                Status::GUATIAO_ERR_GONE,
                format!(
                    "{}: the ssh program could not start: {error}",
                    link.target()
                ),
            )
        })?;
        let (Some(stdin), Some(mut stdout), Some(mut stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            return Err(refuse(
                Status::GUATIAO_ERR_INTERNAL,
                "the ssh program has no pipes",
            ));
        };
        let queue = link.settings.queue_chunks;
        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(queue);
        let (err_tx, err_rx) = mpsc::channel::<Vec<u8>>(queue);
        // Under a terminal the host merges the program's errors into its output; what ssh itself says goes there too.
        let merged = if open.pty.is_some() {
            out_tx.clone()
        } else {
            err_tx.clone()
        };
        // The last thing ssh itself said, kept for an exit status that may be its own.
        let said = Arc::new(Mutex::new(String::new()));
        let after_exit = merged.clone();
        let readers = [
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 8192];
                while let Ok(count @ 1..) = stdout.read(&mut buffer).await {
                    if out_tx.send(buffer[..count].to_vec()).await.is_err() {
                        break;
                    }
                }
            }),
            tokio::spawn({
                let said = Arc::clone(&said);
                async move {
                    let mut buffer = vec![0_u8; 8192];
                    while let Ok(count @ 1..) = stderr.read(&mut buffer).await {
                        let text = String::from_utf8_lossy(&buffer[..count]);
                        if let Some(line) = text.lines().rev().find(|line| !line.trim().is_empty())
                        {
                            *lock(&said) = line.trim().to_owned();
                        }
                        if merged.send(buffer[..count].to_vec()).await.is_err() {
                            break;
                        }
                    }
                }
            }),
        ];
        drop(err_tx);
        let remote = Arc::new(Remote::default());
        let kill = Arc::new(tokio::sync::Notify::new());
        let stop = Arc::new(tokio::sync::Notify::new());
        tokio::spawn({
            let (remote, kill, stop) = (Arc::clone(&remote), Arc::clone(&kill), Arc::clone(&stop));
            let grace = link.settings.exit_grace;
            async move {
                let _permit = permit;
                let mut exited = None;
                let status = tokio::select! {
                    status = child.wait() => status,
                    () = kill.notified() => {
                        let _ = child.start_kill();
                        child.wait().await
                    }
                };
                if let Ok(status) = status {
                    exited = status.code();
                    #[cfg(unix)]
                    let signal =
                        std::os::unix::process::ExitStatusExt::signal(&status).map(i64::from);
                    #[cfg(not(unix))]
                    let signal: Option<i64> = None;
                    remote.finish(status.code().map(i64::from), signal);
                }
                // What the program wrote before it ended is still in the pipes; a descendant may hold them open.
                let ends = async {
                    for reader in readers.iter() {
                        let _ = tokio::time::timeout(grace, async {
                            while !reader.is_finished() {
                                tokio::time::sleep(Duration::from_millis(10)).await;
                            }
                        })
                        .await;
                    }
                };
                tokio::select! { () = ends => {}, () = stop.notified() => {} }
                // 255 is what ssh itself exits with when it fails, and a program may exit with it too: say which it may be.
                if exited == Some(255) {
                    let line = lock(&said).clone();
                    let note = format!(
                        "bohio: ssh exited 255, which is also ssh's own failure status, so it may not be the program's. ssh said: {line}\n"
                    );
                    let _ = after_exit.send(note.into_bytes()).await;
                }
                for reader in &readers {
                    reader.abort();
                }
                remote.finish(None, None);
            }
        });
        if !open.stdin {
            // No input: the program sees the end of it at once.
        }
        let stdin = if open.stdin { Some(stdin) } else { None };
        Ok(Self {
            link: Arc::clone(link),
            backend: Backend::Child(ChildCtl {
                stdin: tokio::sync::Mutex::new(stdin),
                kill,
            }),
            out: Stream::new(out_rx),
            err: Stream::new(err_rx),
            remote,
            killed: AtomicBool::new(false),
            stop,
        })
    }
}

/// What the task that reads a channel holds.
struct Pump {
    stop: Arc<tokio::sync::Notify>,
    out: Sender<Vec<u8>>,
    err: Sender<Vec<u8>>,
    merge: bool,
    remote: Arc<Remote>,
    grace: Duration,
    _permit: OwnedSemaphorePermit,
}

/// Moves a channel's output to the queues and records how the program ended. Holds the channel's slot until the channel
/// is over.
async fn pump(mut read: ChannelReadHalf, ends: Pump) {
    let Pump {
        stop,
        out,
        err,
        merge,
        remote,
        grace,
        _permit,
    } = ends;
    let mut exited: Option<tokio::time::Instant> = None;
    loop {
        let waiting = async {
            match exited {
                Some(at) => tokio::time::timeout_at(at + grace, read.wait())
                    .await
                    .unwrap_or(None),
                None => read.wait().await,
            }
        };
        let message = tokio::select! {
            () = stop.notified() => break,
            message = waiting => message,
        };
        match message {
            None | Some(ChannelMsg::Close) => break,
            Some(ChannelMsg::Data { data }) => {
                if out.send(data.to_vec()).await.is_err() {
                    break;
                }
            }
            Some(ChannelMsg::ExtendedData { data, .. }) => {
                let to = if merge { &out } else { &err };
                if to.send(data.to_vec()).await.is_err() {
                    break;
                }
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => {
                remote.finish(Some(i64::from(exit_status)), None);
                exited.get_or_insert_with(tokio::time::Instant::now);
            }
            Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                remote.finish(None, signal_number(sig_name(&signal_name)));
                exited.get_or_insert_with(tokio::time::Instant::now);
            }
            Some(_) => {}
        }
    }
    remote.finish(None, None);
}

fn sig_name(signal: &Sig) -> &str {
    match signal {
        Sig::ABRT => "ABRT",
        Sig::ALRM => "ALRM",
        Sig::FPE => "FPE",
        Sig::HUP => "HUP",
        Sig::ILL => "ILL",
        Sig::INT => "INT",
        Sig::KILL => "KILL",
        Sig::PIPE => "PIPE",
        Sig::QUIT => "QUIT",
        Sig::SEGV => "SEGV",
        Sig::TERM => "TERM",
        Sig::USR1 => "USR1",
        Sig::Custom(name) => name,
    }
}
