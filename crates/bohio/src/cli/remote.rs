//! `bohio remote`: use a host over a proven link.

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use bohio_client::Error;
use bohio_client::connect::Connection;
use bohio_client::control::Control;
use bohio_client::dial::{ServerTrust, split_command};
use bohio_client::exec::{Event, Exec};
use bohio_client::fs::Reader;
use bohio_client::shell::{Event as ShellEvent, Shell};
use bohio_proto::exec::{Request, Stdin};
use bohio_proto::shell::{Request as ShellRequest, ShellKind};
use coqui_link::auth::{Proof, Trust};
use coqui_peer::PeerId;

use super::commands::{print_info, source};
use super::{Format, RemoteAction, RemoteArgs, RemoteExecArgs, RemoteShellArgs};
use crate::config::Environment;
use crate::serve::Endpoint;

/// What a person is told when the host will not do what was asked.
fn explain(error: &Error) -> String {
    match error {
        Error::Remote(body) => format!("the host refused: {}", body.message),
        Error::Transport(coqui_transport::Failure::Permission) => {
            "the host refused or ended the stream: this peer may not do that, or its access was revoked".to_owned()
        }
        other => other.to_string(),
    }
}

fn connect(args: &RemoteArgs, environment: &Environment) -> Result<Connection, String> {
    let (config, data) = source(&args.config, environment)?;
    let endpoint = match (&args.endpoint, &args.command) {
        (Some(text), None) => Endpoint::parse(text)?,
        (None, Some(command)) => Endpoint::Command(split_command(command)?),
        _ => return Err("name the host as an endpoint or with --command, not both".to_owned()),
    };
    let identity = data.identity().map_err(|error| error.to_string())?;
    let trust = match &args.server {
        Some(id) => Trust::pinned([id
            .parse::<PeerId>()
            .map_err(|_| "that is not a peer id".to_owned())?]),
        None => Trust::Store(data.store().map_err(|error| error.to_string())?),
    };
    let proof = Proof::builder(identity)
        .responders(trust)
        .build()
        .map_err(|error| error.to_string())?;
    let settings = config.link.settings();
    let mut trust = ServerTrust::default();
    trust.ca_files.clone_from(&args.ca);
    for hash in &args.cert_hash {
        trust.pin(hash)?;
    }
    if matches!(&endpoint, Endpoint::WebSocket(url) if url.scheme() == "wss") {
        trust
            .client_config()
            .map_err(|reason| format!("the TLS trust cannot be set up: {reason}"))?;
    }
    Connection::connect_with(&endpoint, settings.proof_timeout, &proof, &settings, &trust).map_err(
        |error| match error {
            Error::Transport(coqui_transport::Failure::Certificate) => format!(
                "the host's TLS certificate was refused: trust its issuer with --ca FILE, or the                  certificate itself with --cert-hash HASH (endpoint {endpoint})"
            ),
            Error::Transport(_) | Error::Closed => format!(
                "the link to {endpoint} ended before it was proven: the host may not be listening, \
                 may not trust this key yet (it waits for `bohio peer approve`), or this side does \
                 not trust the host (`bohio join`, or --server ID)"
            ),
            other => other.to_string(),
        },
    )
}

/// `bohio remote`.
pub(super) fn run(args: &RemoteArgs, environment: &Environment) -> Result<ExitCode, String> {
    let link = connect(args, environment)?;
    match &args.action {
        RemoteAction::Info(info) => {
            let control = Control::open(&link).map_err(|error| explain(&error))?;
            let found = control
                .info(Some(Duration::from_secs(30)))
                .map_err(|error| explain(&error))?;
            match info.format {
                Format::Text => print_info(&found),
                Format::Json => println!(
                    "{}",
                    serde_json::to_string(&found)
                        .map_err(|_| "the capabilities cannot be written")?
                ),
            }
            Ok(ExitCode::SUCCESS)
        }
        RemoteAction::Exec(exec) => exec_command(&link, exec),
        RemoteAction::Read(read) => {
            let reader = Reader::start(
                &link,
                &bohio_proto::fs::ReadRequest::whole(read.path.clone()),
                Some(Duration::from_secs(30)),
            )
            .map_err(|error| explain(&error))?;
            let mut out = std::io::stdout().lock();
            reader
                .copy_to(&mut out, None)
                .map_err(|error| explain(&error))?;
            out.flush().map_err(|error| error.to_string())?;
            Ok(ExitCode::SUCCESS)
        }
        RemoteAction::Shell(shell) => shell_command(&link, shell),
    }
}

/// The exit status a finished program gives this command.
fn status_of(exit: &bohio_proto::exec::Exit) -> ExitCode {
    let code = if exit.timed_out {
        124
    } else if let Some(code) = exit.code {
        code & 0xff
    } else if let Some(signal) = exit.signal {
        128 + signal
    } else {
        1
    };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

fn exec_command(link: &Connection, args: &RemoteExecArgs) -> Result<ExitCode, String> {
    let mut request = Request::argv(args.argv.iter().cloned());
    request.cwd.clone_from(&args.cwd);
    request.timeout_ms = args.timeout_ms;
    for pair in &args.env {
        let (name, value) = pair
            .split_once('=')
            .ok_or_else(|| "--env is NAME=VALUE".to_owned())?;
        request.env.set.insert(name.to_owned(), value.to_owned());
    }
    if args.stdin {
        request.stdin = Stdin::Open;
    }
    let exec = Arc::new(Exec::start(link, &request).map_err(|error| explain(&error))?);
    if args.stdin {
        // Detached: it may be blocked reading this process's input when the program ends.
        let exec = Arc::clone(&exec);
        thread::spawn(move || {
            let mut buffer = vec![0_u8; 16 * 1024];
            let mut input = std::io::stdin().lock();
            while let Ok(count) = input.read(&mut buffer) {
                if count == 0 || exec.write_stdin(&buffer[..count]).is_err() {
                    break;
                }
            }
            let _ = exec.finish_stdin();
        });
    }
    loop {
        match exec.next(None) {
            Ok(Some(Event::Stdout(bytes))) => {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&bytes);
                let _ = out.flush();
            }
            Ok(Some(Event::Stderr(bytes))) => {
                let mut err = std::io::stderr().lock();
                let _ = err.write_all(&bytes);
                let _ = err.flush();
            }
            Ok(Some(Event::Exit(exit))) => return Ok(status_of(&exit)),
            Ok(Some(_)) => {}
            Ok(None) => return Err("the stream ended before the program's exit".to_owned()),
            Err(error) => return Err(explain(&error)),
        }
    }
}

/// The terminal size to ask for: the local one when there is one.
fn local_size() -> (u16, u16) {
    crossterm::terminal::size().unwrap_or((80, 24))
}

fn shell_command(link: &Connection, args: &RemoteShellArgs) -> Result<ExitCode, String> {
    let default = if cfg!(windows) {
        ShellKind::Cmd
    } else {
        ShellKind::Sh
    };
    let kind = match &args.kind {
        Some(name) => ShellKind::KNOWN
            .into_iter()
            .find(|kind| kind.program() == name)
            .ok_or_else(|| "a shell is one of sh, bash, zsh, cmd, powershell, pwsh".to_owned())?,
        None => default,
    };
    let mut request = ShellRequest::kind(kind);
    request.cwd.clone_from(&args.cwd);
    let interactive = std::io::stdin().is_terminal();
    let mut size = if interactive { local_size() } else { (80, 24) };
    request.cols = size.0;
    request.rows = size.1;
    let shell = Arc::new(
        Shell::start(link, &request, Some(Duration::from_secs(30)))
            .map_err(|error| explain(&error))?,
    );
    // Typed keys go to the host as they are, so the local terminal must not echo or edit them.
    let raw = interactive && crossterm::terminal::enable_raw_mode().is_ok();
    {
        // Detached: it is blocked reading this process's input when the shell ends.
        let shell = Arc::clone(&shell);
        thread::spawn(move || {
            let mut buffer = vec![0_u8; 4096];
            let mut input = std::io::stdin().lock();
            while let Ok(count) = input.read(&mut buffer) {
                if count == 0 || shell.write(&buffer[..count]).is_err() {
                    break;
                }
            }
        });
    }
    let result = loop {
        match shell.next(Some(Duration::from_millis(200))) {
            Ok(Some(ShellEvent::Data(bytes))) => {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&bytes);
                let _ = out.flush();
            }
            Ok(Some(ShellEvent::Exit(exit))) => break Ok(exit),
            Ok(Some(_)) => {}
            Ok(None) => break Err("the stream ended before the shell's exit".to_owned()),
            Err(Error::Timeout) => {
                if interactive {
                    let now = local_size();
                    if now != size {
                        size = now;
                        let _ = shell.resize(now.0, now.1);
                    }
                }
            }
            Err(error) => break Err(explain(&error)),
        }
    };
    if raw {
        let _ = crossterm::terminal::disable_raw_mode();
    }
    let exit = result?;
    Ok(ExitCode::from(
        exit.code
            .and_then(|code| u8::try_from(code & 0xff).ok())
            .unwrap_or(0),
    ))
}
