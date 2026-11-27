// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `bohio-provider-ssh`: a machine reached over SSH as Bohío sources, with nothing of Bohío installed on it.
//!
//! One source is one connection to one host; channels are opened on it as peers ask. It serves the machine's terminal
//! (a pseudo-terminal that runs its login shell or a program) and its processes. A source that cannot connect is
//! reported unavailable with the reason and tried again after `reconnect_after`; building the source never touches the
//! network. Everything the account on the host can do, a peer holding the source can do.

#![forbid(unsafe_code)]

mod agent;
mod command;
mod desktop;
mod files;
mod forward;
mod link;
mod openssh;
mod session;
mod settings;

use std::sync::Arc;
use std::time::Instant;

use bohio_api::guatiao::library::{Object, ProviderError};
use bohio_api::guatiao::{Provider, Status};
use bohio_api::model::{Delivery, ExecStart, SourceInfo, TerminalExit, TerminalOpen};
use bohio_api::{
    DesktopSession, DesktopSource, Directory, ExecSource, FilesSource, Process, Terminal,
    TerminalSource, vocab,
};
use bohio_x11::source::DesktopOf;
use russh::Sig;

use crate::link::{Link, Rt, refuse};
use crate::session::{Open, Session};
pub use crate::settings::SshSettings;

/// The provider's id: `[source.<name>] provider = "ssh"`.
pub const ID: &str = "ssh";

/// A machine reached over SSH.
#[derive(Default, Provider)]
#[provider(
    TerminalSource,
    ExecSource,
    FilesSource,
    DesktopSource,
    id = "ssh",
    name = "ssh",
    crate = ::bohio_api::guatiao,
    config = SshSettings
)]
pub struct Ssh {
    link: Option<Arc<Link>>,
    /// Whether the login shell read the probe line as POSIX does, once it was asked and the answer was the shell's own.
    posix: std::sync::OnceLock<bool>,
    files: Option<crate::files::SftpFiles>,
    desktop: Option<Arc<DesktopOf>>,
}

impl TryFrom<SshSettings> for Ssh {
    type Error = ProviderError;

    /// Checks the settings and starts the runtime; nothing touches the network.
    fn try_from(settings: SshSettings) -> Result<Self, Self::Error> {
        let read = settings.read()?;
        for line in settings.non_default() {
            tracing::warn!("non-default: ssh source {}: {line}", read.host);
        }
        let rt = Rt::new(read.worker_threads)?;
        let display = read.x11_display;
        let link = Link::new(read, rt);
        let desktop = display.map(|display| {
            Arc::new(DesktopOf::new(Arc::new(crate::desktop::desktop(
                &link, display,
            ))))
        });
        Ok(Self {
            posix: std::sync::OnceLock::new(),
            desktop,
            files: Some(crate::files::SftpFiles(crate::files::Files::new(
                Arc::clone(&link),
            ))),
            link: Some(link),
        })
    }
}

impl Ssh {
    fn link(&self) -> Result<&Arc<Link>, ProviderError> {
        self.link
            .as_ref()
            .ok_or_else(|| ProviderError::new(Status::GUATIAO_ERR_NULL, "no host is configured"))
    }

    /// Refuses a command line that carries words until the account's login shell has been seen to read quotes as a POSIX
    /// shell does (`login_shell_check`); the probe runs once for the source.
    fn require_posix_shell(&self) -> Result<(), ProviderError> {
        let link = self.link()?;
        if !link.settings.login_shell_check {
            return Ok(());
        }
        let posix = match self.posix.get() {
            Some(posix) => *posix,
            None => {
                // A failure to log in or to open a channel is that failure, not an answer about the shell, and a probe
                // that did not end says nothing about it either: neither is kept, and the next start asks again.
                let posix = match self.probe_login_shell(link)? {
                    command::Probe::Posix => true,
                    command::Probe::Other => false,
                    command::Probe::Unknown => {
                        return Err(bohio_api::unavailable(
                            "the account's login shell did not answer the probe in time, so it is not known to read a command line as a POSIX shell does; try again (`login_shell_check = false` skips this check)",
                        ));
                    }
                };
                *self.posix.get_or_init(|| posix)
            }
        };
        if posix {
            Ok(())
        } else {
            Err(ProviderError::new(
                Status::GUATIAO_ERR_UNSUPPORTED,
                "the account's login shell does not read a command line as a POSIX shell does, so a program's words cannot be quoted for it; use an account whose login shell is sh, bash or zsh (`login_shell_check = false` skips this check)",
            ))
        }
    }

    /// Runs the probe line and reads what it prints.
    fn probe_login_shell(&self, link: &Arc<Link>) -> Result<command::Probe, ProviderError> {
        let session = link.rt.block_on(Session::start(
            link,
            Open {
                pty: None,
                command: Some(command::PROBE_LINE.to_owned()),
                stdin: false,
            },
        ))?;
        let deadline = Instant::now() + link.settings.operation_timeout;
        let left = || {
            u64::try_from(
                deadline
                    .saturating_duration_since(Instant::now())
                    .as_millis(),
            )
            .unwrap_or(u64::MAX)
        };
        let (mut output, mut chunk) = (Vec::new(), [0_u8; 256]);
        // Read to the end of the output: an rc file may print before the line does. Only the tail is kept.
        loop {
            let wait = left();
            if wait == 0 {
                break;
            }
            match session.read(1, &mut chunk, wait) {
                Ok(count) if count > 0 => {
                    output.extend_from_slice(&chunk[..usize::try_from(count).unwrap_or(0)]);
                    if output.len() > command::PROBE_KEEP {
                        output.drain(..output.len() - command::PROBE_KEEP);
                    }
                }
                _ => break,
            }
        }
        // Let the channel end before the program's own starts, so the probe holds none of the server's sessions.
        let ended = session.wait(left()).is_some();
        let _ = session.kill();
        Ok(command::probe_verdict(&output, ended))
    }

    fn sftp(&self) -> Result<&crate::files::SftpFiles, ProviderError> {
        self.files
            .as_ref()
            .ok_or_else(|| ProviderError::new(Status::GUATIAO_ERR_NULL, "no host is configured"))
    }

    /// Whether the host can be reached now, and why not.
    fn reach(&self) -> Result<(), String> {
        let link = self.link().map_err(|error| error.message().to_string())?;
        let wait = link.settings.connect_timeout + link.settings.operation_timeout;
        if link.settings.openssh.is_some() {
            return link.rt.block_on(crate::openssh::probe(link, wait));
        }
        match link
            .rt
            .block_on(async { tokio::time::timeout(wait, link.conn()).await })
        {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(error.message().to_string()),
            Err(_) => Err(format!("{}: no answer in time", link.target())),
        }
    }

    fn info(&self, offers: &[&str], lacks: &[&str]) -> SourceInfo {
        // A source that hands its programs the node's display says so, for the node to hold such a start to the GUI grants.
        let forwards = self.link().is_ok_and(|link| link.settings.x11_forward);
        let mut offers = offers.to_vec();
        if forwards {
            offers.push(vocab::exec::X11_FORWARD);
        }
        let info = SourceInfo::new(ID, &offers, lacks);
        match self.reach() {
            Ok(()) => info,
            Err(reason) => info.unavailable(&reason),
        }
    }
}

impl DesktopSource for Ssh {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        let Some(desktop) = &self.desktop else {
            return Ok(SourceInfo::new(
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
            .unavailable("this source has no x11_display, so it serves no desktop"));
        };
        let mut info = desktop.describe()?;
        ID.clone_into(&mut info.provider);
        // The host's login session is not asked: a locked screen is not told.
        if !info
            .lacks
            .iter()
            .any(|word| word == vocab::desktop::LOCK_DETECTION)
        {
            info.offers
                .retain(|word| word != vocab::desktop::LOCK_DETECTION);
            info.lacks.push(vocab::desktop::LOCK_DETECTION.to_owned());
        }
        Ok(info)
    }

    fn open(&self) -> Result<Object<dyn DesktopSession>, ProviderError> {
        match &self.desktop {
            Some(desktop) => desktop.open(),
            None => Err(bohio_api::unavailable(
                "this source has no x11_display, so it serves no desktop",
            )),
        }
    }
}

impl TerminalSource for Ssh {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        use vocab::terminal as word;
        // The system's ssh client cannot tell the host a new size and sends no signal request, so the one signal a
        // terminal takes there is `interrupt`, as the control byte; any other is `unsupported`.
        if self.link()?.settings.openssh.is_some() {
            return Ok(self.info(
                &[
                    word::READ,
                    word::WRITE,
                    word::PROGRAMS,
                    word::SIGNALS,
                    word::EXIT_STATUS,
                ],
                &[word::BREAK, word::RESIZE],
            ));
        }
        Ok(self.info(
            &[
                word::READ,
                word::WRITE,
                word::PROGRAMS,
                word::RESIZE,
                word::SIGNALS,
                word::EXIT_STATUS,
            ],
            &[word::BREAK],
        ))
    }

    fn open(&self, request: TerminalOpen) -> Result<Object<dyn Terminal>, ProviderError> {
        let link = self.link()?;
        let bad = |why: String| refuse(Status::GUATIAO_ERR_BAD_VALUE, why);
        let cwd = request.cwd.as_deref();
        if request.program.is_some()
            || cwd.is_some()
            || request.clear_env
            || !request.env.is_empty()
        {
            self.require_posix_shell()?;
        }
        let command = match request.program.as_ref().map(command::terminal_words) {
            Some(words) => {
                let words = words.map_err(bad)?.unwrap_or_default();
                command::line(&words, cwd, request.clear_env, &request.env).map_err(bad)?
            }
            None if cwd.is_some() || request.clear_env || !request.env.is_empty() => {
                command::login_shell_line(cwd, request.clear_env, &request.env).map_err(bad)?
            }
            None => String::new(),
        };
        let term = request
            .env
            .iter()
            .find(|change| change.name == "TERM")
            .and_then(|change| change.value.clone())
            .unwrap_or_else(|| "xterm-256color".to_owned());
        let open = Open {
            pty: Some((term, u32::from(request.cols), u32::from(request.rows))),
            command: (!command.is_empty()).then_some(command),
            stdin: true,
        };
        let session = link.rt.block_on(Session::start(link, open))?;
        Ok(Term(session).into_object())
    }
}

impl FilesSource for Ssh {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        Ok(self.sftp()?.describe())
    }

    fn open_dir(&self, path: &str) -> Result<Object<dyn Directory>, ProviderError> {
        FilesSource::open_dir(self.sftp()?, path)
    }

    fn rename_across(
        &self,
        from_root: &str,
        from: &str,
        to_root: &str,
        to: &str,
        overwrite: bool,
    ) -> Result<(), ProviderError> {
        FilesSource::rename_across(self.sftp()?, from_root, from, to_root, to, overwrite)
    }
}

impl ExecSource for Ssh {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        use vocab::exec as word;
        if self.link()?.settings.openssh.is_some() {
            return Ok(self.info(
                &[
                    word::START,
                    word::STDIN,
                    word::STDERR,
                    word::SHELL,
                    word::CWD,
                    word::ENV,
                    word::LISTED,
                    word::EXIT_STATUS,
                ],
                &[word::KILL_TREE, word::SIGNALS],
            ));
        }
        Ok(self.info(
            &[
                word::START,
                word::STDIN,
                word::STDERR,
                word::SIGNALS,
                word::SHELL,
                word::CWD,
                word::ENV,
                word::LISTED,
                word::EXIT_STATUS,
            ],
            &[word::KILL_TREE],
        ))
    }

    fn start(&self, request: ExecStart) -> Result<Object<dyn Process>, ProviderError> {
        let link = self.link()?;
        let bad = |why: String| refuse(Status::GUATIAO_ERR_BAD_VALUE, why);
        let words = command::exec_words(&request).map_err(bad)?;
        self.require_posix_shell()?;
        let command = command::line(
            &words,
            request.cwd.as_deref(),
            request.clear_env,
            &request.env,
        )
        .map_err(bad)?;
        let session = link.rt.block_on(Session::start(
            link,
            Open {
                pty: None,
                command: Some(command),
                stdin: request.stdin,
            },
        ))?;
        Ok(Proc(session).into_object())
    }
}

/// A program running on the host, as a process.
struct Proc(Session);

/// A program running on the host, on a terminal.
struct Term(Session);

fn sig_of(name: &str) -> Option<(Sig, bool)> {
    Some(match name {
        vocab::signal::INTERRUPT => (Sig::INT, false),
        vocab::signal::TERMINATE => (Sig::TERM, false),
        vocab::signal::KILL => (Sig::KILL, true),
        vocab::signal::HANGUP => (Sig::HUP, false),
        vocab::signal::QUIT => (Sig::QUIT, false),
        vocab::signal::USER1 => (Sig::USR1, false),
        vocab::signal::USER2 => (Sig::Custom("USR2".to_owned()), false),
        _ => return None,
    })
}

impl Process for Proc {
    fn read(&self, stream: u8, dst: &mut [u8], wait_ms: u64) -> Result<i64, ProviderError> {
        self.0.read(stream, dst, wait_ms)
    }

    fn write(&self, data: &[u8]) -> Result<u64, ProviderError> {
        self.0.write(data)
    }

    fn close_input(&self) -> Result<(), ProviderError> {
        self.0.close_input()
    }

    fn wait(&self, wait_ms: u64) -> Result<Option<TerminalExit>, ProviderError> {
        Ok(self.0.wait(wait_ms))
    }

    fn kill(&self) -> Result<TerminalExit, ProviderError> {
        Ok(self.0.kill())
    }

    fn signal(&self, name: &str) -> Result<Delivery, ProviderError> {
        let (sig, ends) =
            sig_of(name).ok_or_else(|| refuse(Status::GUATIAO_ERR_BAD_VALUE, "not a signal"))?;
        self.0.signal(sig, ends)
    }
}

impl Terminal for Term {
    fn read(&self, dst: &mut [u8], wait_ms: u64) -> Result<i64, ProviderError> {
        self.0.read(1, dst, wait_ms)
    }

    fn write(&self, data: &[u8]) -> Result<u64, ProviderError> {
        self.0.write(data).or_else(|error| {
            // A write to a program that has ended takes nothing; `exit` says why.
            if self.0.exit().is_some() {
                Ok(0)
            } else {
                Err(error)
            }
        })
    }

    fn kill(&self) -> Result<TerminalExit, ProviderError> {
        Ok(self.0.kill())
    }

    fn exit(&self) -> Result<Option<TerminalExit>, ProviderError> {
        Ok(self.0.exit())
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<(), ProviderError> {
        self.0.resize(cols, rows)
    }

    fn signal(&self, name: &str) -> Result<Delivery, ProviderError> {
        if name == "interrupt" {
            // On a terminal the interrupt is what the line discipline makes of the control byte, for the program in
            // the foreground, whatever the host does with an SSH signal request.
            self.0.control(0x03)?;
            return Ok(Delivery {
                delivered: true,
                ended: false,
                message: None,
            });
        }
        let (sig, ends) =
            sig_of(name).ok_or_else(|| refuse(Status::GUATIAO_ERR_BAD_VALUE, "not a signal"))?;
        self.0.signal(sig, ends)
    }
}

bohio_api::guatiao::local_providers!(
    id = "bohio-ssh",
    version = env!("CARGO_PKG_VERSION"),
    providers = [Ssh]
);
