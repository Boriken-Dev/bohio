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
    write: Arc<ChannelWriteHalf<Msg>>,
    out: Mutex<Stream>,
    err: Mutex<Stream>,
    remote: Arc<Remote>,
    killed: AtomicBool,
    /// Lets go of the channel's slot without waiting for the host to answer a close.
    stop: Arc<tokio::sync::Notify>,
}

impl Session {
    pub(crate) async fn start(link: &Arc<Link>, open: Open) -> Result<Self, ProviderError> {
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
            write: Arc::new(write),
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
        within(
            &self.link,
            self.link.settings.operation_timeout,
            self.write.data_bytes(data.to_vec()),
        )?
        .map(|()| data.len() as u64)
        .map_err(|error| unavailable(&format!("{}: {error}", self.link.target())))
    }

    pub(crate) fn close_input(&self) -> Result<(), ProviderError> {
        within(
            &self.link,
            self.link.settings.operation_timeout,
            self.write.eof(),
        )?
        .map_err(|error| unavailable(&format!("{}: {error}", self.link.target())))
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
            let write = Arc::clone(&self.write);
            let _ = within(
                &self.link,
                self.link.settings.operation_timeout,
                async move {
                    let _ = write.signal(Sig::KILL).await;
                    let _ = write.close().await;
                },
            );
        }
        self.remote.finish(None, signal_number("KILL"));
        self.stop.notify_one();
        self.remote.snapshot().unwrap_or_default()
    }

    pub(crate) fn resize(&self, cols: u16, rows: u16) -> Result<(), ProviderError> {
        within(
            &self.link,
            self.link.settings.operation_timeout,
            self.write
                .window_change(u32::from(cols), u32::from(rows), 0, 0),
        )?
        .map_err(|error| unavailable(&format!("{}: {error}", self.link.target())))
    }

    /// Sends the SSH signal `sig`; the host decides whether it reaches the program.
    pub(crate) fn signal(&self, sig: Sig, ends: bool) -> Result<Delivery, ProviderError> {
        let running = self.remote.snapshot().is_none();
        within(
            &self.link,
            self.link.settings.operation_timeout,
            self.write.signal(sig),
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
            let write = Arc::clone(&self.write);
            self.link.rt.handle().spawn(async move {
                let _ = write.signal(Sig::KILL).await;
                let _ = write.close().await;
            });
        }
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
