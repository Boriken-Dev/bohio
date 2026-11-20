//! `bohio remote`: use a host over a proven link.

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use bohio_client::connect::Connection;
use bohio_client::control::Control;
use bohio_client::dial::{ServerTrust, split_command};
use bohio_client::exec::{Event, Exec};
use bohio_client::fs::Reader;
use bohio_client::hash::hash_many;
use bohio_client::shell::{Event as ShellEvent, Shell};
use bohio_client::{Error, Open};
use bohio_proto::exec::{Request, Stdin};
use bohio_proto::hash::{Algorithm, HashItem};
use bohio_proto::shell::{Request as ShellRequest, ShellKind};
use coqui_link::auth::{Proof, Trust};
use coqui_peer::PeerId;

use super::commands::{print_info, source};
use super::{Format, RemoteAction, RemoteArgs, RemoteExecArgs, RemoteHashArgs, RemoteShellArgs};
use crate::config::{Environment, Layers};
use crate::local::{ANOTHER_ACCOUNT, Server, Via};
use crate::node::Endpoint;

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
    let (found, data) = source(&args.config, environment)?;
    let config = found.config();
    let endpoint = match (&args.endpoint, &args.command) {
        (Some(text), None) => Endpoint::parse(text)?,
        (None, Some(command)) => Endpoint::Command(split_command(command)?),
        _ => return Err("name the host as an endpoint or with --command, not both".to_owned()),
    };
    let endpoint = match &args.fallback {
        Some(text) => endpoint.with_fallback(Endpoint::parse(text)?)?,
        None => endpoint,
    };
    let identity = data.identity().map_err(|error| error.to_string())?;
    let trust = match &args.server {
        Some(id) => Trust::pinned([id
            .parse::<PeerId>()
            .map_err(|_| "that is not a peer id".to_owned())?]),
        // The keys `bohio join` or `bohio peer approve` wrote into the layers: a host this node trusts.
        None => Trust::pinned(
            Layers::peers(&found.watch)
                .map_err(|error| error.to_string())?
                .ids()
                .filter_map(|name| name.parse::<PeerId>().ok()),
        ),
    };
    let proof = Proof::builder(identity)
        .responders(trust)
        .build()
        .map_err(|error| error.to_string())?;
    let settings = config.link.settings();
    let trust = tls_trust(&endpoint, &args.ca, &args.cert_hash, args.clock_skew)?;
    Connection::connect_with(&endpoint, settings.proof_timeout, &proof, &settings, &trust).map_err(
        |error| match error {
            Error::Transport(coqui_transport::Failure::Certificate) => {
                certificate_refused(&endpoint, &trust)
            }
            Error::Transport(_) | Error::Closed => format!(
                "the link to {endpoint} ended before it was proven: the host may not be listening, \
                 may not trust this key yet (it waits for `bohio peer approve`), or this side does \
                 not trust the host (`bohio join`, or --server ID)"
            ),
            other => other.to_string(),
        },
    )
}

/// How the TLS host of `endpoint` is trusted: the PEM files, the hashes and the clock difference the operator gave. The set is
/// checked now, so a bad file is named before anything connects.
pub(super) fn tls_trust(
    endpoint: &Endpoint,
    ca: &[std::path::PathBuf],
    cert_hash: &[String],
    clock_skew: Option<Duration>,
) -> Result<ServerTrust, String> {
    let mut trust = ServerTrust::default();
    trust.ca_files = ca.to_vec();
    trust.clock_skew = clock_skew.unwrap_or_default();
    for hash in cert_hash {
        trust.pin(hash)?;
    }
    let needs_tls = match endpoint {
        Endpoint::WebSocket(url) => url.scheme() == "wss",
        Endpoint::WebTransport { .. } => true,
        _ => false,
    };
    if needs_tls {
        trust
            .client_config()
            .map_err(|reason| format!("the TLS trust cannot be set up: {reason}"))?;
    }
    Ok(trust)
}

/// What a person is told when the host's TLS certificate is refused.
pub(super) fn certificate_refused(endpoint: &Endpoint, trust: &ServerTrust) -> String {
    format!(
        "the host's TLS certificate was refused ({endpoint}): {}",
        trust.refusal().unwrap_or_else(|| {
            "trust its issuer with --ca FILE, or the certificate itself with --cert-hash HASH"
                .to_owned()
        })
    )
}

/// `bohio remote`.
pub(super) fn run(args: &RemoteArgs, environment: &Environment) -> Result<ExitCode, String> {
    let link: Box<dyn Open> = match &args.via {
        Some(id) => Box::new(connect_via(id, args, environment)?),
        None => Box::new(connect(args, environment)?),
    };
    match &args.action {
        RemoteAction::Info(info) => {
            let control = Control::open(&*link).map_err(|error| explain(&error))?;
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
        RemoteAction::Exec(exec) => exec_command(&*link, exec),
        RemoteAction::Read(read) => {
            let reader = Reader::start(
                &*link,
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
        RemoteAction::Hash(hash) => hash_command(&*link, hash),
        RemoteAction::Shell(shell) => shell_command(&*link, shell),
    }
}

/// `bohio remote ... hash`: one line per file, in the order given; a file that cannot be hashed is reported and
/// the exit status is 1 after the rest are printed.
fn hash_command(link: &dyn Open, args: &RemoteHashArgs) -> Result<ExitCode, String> {
    let mut algorithms: Vec<Algorithm> = Vec::new();
    for name in &args.algorithm {
        let algorithm = Algorithm::from_name(name);
        if !algorithm.is_known() {
            let known: Vec<&str> = Algorithm::KNOWN.iter().map(Algorithm::name).collect();
            return Err(format!(
                "`{name}` is not an algorithm; the names are {}",
                known.join(", ")
            ));
        }
        if !algorithms.contains(&algorithm) {
            algorithms.push(algorithm);
        }
    }
    if algorithms.is_empty() {
        algorithms.push(Algorithm::Sha256);
    }
    let items: Vec<HashItem> = args.paths.iter().map(HashItem::whole).collect();
    let answers = hash_many(link, &items, &algorithms).map_err(|error| explain(&error))?;
    let mut failed = false;
    let mut out = std::io::stdout().lock();
    for (path, answer) in args.paths.iter().zip(answers) {
        match answer {
            Ok(digests) => {
                for algorithm in &algorithms {
                    let hex = digests.get(algorithm).map_or("", String::as_str);
                    let line = if algorithms.len() == 1 {
                        format!("{hex}  {path}")
                    } else {
                        format!("{algorithm} {hex}  {path}")
                    };
                    writeln!(out, "{line}").map_err(|error| error.to_string())?;
                }
            }
            Err(body) => {
                failed = true;
                eprintln!("bohio: {path}: {}", body.message);
            }
        }
    }
    out.flush().map_err(|error| error.to_string())?;
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
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

fn exec_command(link: &dyn Open, args: &RemoteExecArgs) -> Result<ExitCode, String> {
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

fn shell_command(link: &dyn Open, args: &RemoteShellArgs) -> Result<ExitCode, String> {
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

/// The node's local control endpoint for the configuration `config` names: the one set, else the default of
/// this machine's key.
fn control_endpoint(
    config: &super::ConfigOption,
    environment: &Environment,
) -> Result<(Endpoint, bohio_client::link::Settings, Server), String> {
    let (found, data) = source(config, environment)?;
    let identity = data.identity().map_err(|error| error.to_string())?;
    let settings = found.config().link.settings();
    let text = found.config().control.endpoint_for(&identity.peer_id());
    let server = Server::for_control(&found.config().control);
    Endpoint::parse(&text)
        .map(|endpoint| (endpoint, settings, server))
        .map_err(|reason| format!("[control] endpoint: {reason}"))
}

/// What a client of the control endpoint says when it cannot use it: why another account's endpoint is refused,
/// else that no node answers.
fn unreachable(error: &bohio_client::Error) -> String {
    match error {
        bohio_client::Error::Link(text) if text == ANOTHER_ACCOUNT => format!(
            "{text}: another account bound the name first; stop it, then start `bohio serve` again"
        ),
        _ => NO_NODE.to_owned(),
    }
}

const NO_NODE: &str = "the node's local control endpoint does not answer: `bohio serve` must be running with this configuration and `[control] enabled` not false";

/// The link a running node holds to `id`, through its local control endpoint.
fn connect_via(id: &str, args: &RemoteArgs, environment: &Environment) -> Result<Via, String> {
    let peer = id
        .parse::<PeerId>()
        .map_err(|_| "that is not a peer id".to_owned())?;
    let (endpoint, settings, server) = control_endpoint(&args.config, environment)?;
    let via = Via::connect_to(&endpoint, &peer, &settings, server)
        .map_err(|error| unreachable(&error))?;
    let listed = via
        .links(Duration::from_millis(50))
        .map_err(|error| explain(&error))?;
    if !listed
        .links
        .iter()
        .any(|link| link.peer == peer.to_string())
    {
        return Err(format!(
            "no link to {peer}: the node holds none (`bohio links` lists them); a peer that dials in must have connected"
        ));
    }
    Ok(via)
}

/// `bohio links`.
pub(super) fn links(args: &super::LinksArgs, environment: &Environment) -> Result<(), String> {
    let wait = crate::config::Span::parse(&args.wait)
        .ok_or_else(|| "--wait is a time such as `5s` or `500ms`".to_owned())?
        .0;
    let (endpoint, settings, server) = control_endpoint(&args.config, environment)?;
    let local = crate::local::LocalLink::connect_to(&endpoint, &settings, server)
        .map_err(|error| unreachable(&error))?;
    let reply = local.links(wait).map_err(|error| explain(&error))?;
    match args.format {
        Format::Json => println!(
            "{}",
            serde_json::to_string(&reply).map_err(|_| "the links cannot be written")?
        ),
        Format::Text => {
            if reply.links.is_empty() {
                println!("no links");
            }
            for link in &reply.links {
                let at = link
                    .endpoint
                    .as_deref()
                    .or(link.source.as_deref())
                    .unwrap_or("-");
                let list = |offers: &[String]| {
                    if offers.is_empty() {
                        "none".to_owned()
                    } else {
                        offers.join(",")
                    }
                };
                let theirs = link
                    .theirs
                    .as_deref()
                    .map_or_else(|| "unknown".to_owned(), list);
                println!(
                    "{} {} {at} offers-to-peer: {} offers-from-peer: {theirs}",
                    link.peer,
                    link.side,
                    list(&link.mine)
                );
            }
        }
    }
    Ok(())
}
