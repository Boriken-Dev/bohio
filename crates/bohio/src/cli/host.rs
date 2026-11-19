//! The commands that run or manage this host: `serve`, `status`, `id`, `peer`, `pair`, `join`.

use std::io::{BufRead, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::Duration;

use bohio_client::link::socket;
use coqui_link::auth::Identity;
use coqui_link::pairing;
use coqui_peer::PeerId;
use coqui_peer::pair::{CodeAlphabet, Pin};
use coqui_transport::Channel;

use super::commands::{load, source};
use super::{
    ApproveArgs, JoinArgs, PairArgs, PeerAction, PeerArgs, RevokeArgs, ServeArgs, StatusArgs,
};
use crate::config::{Config, Environment, ListenConfig, Span};
use crate::listen::Listening;
use crate::peer;
use crate::policy::{Grant, Permission};
use crate::serve::{Endpoint, Server};

/// Standard output, flushed: a person or a test reads these lines as they happen.
macro_rules! say {
    ($($arg:tt)*) => {{
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, $($arg)*);
        let _ = out.flush();
    }};
}

/// Pairing by PIN meets a stranger over TCP; any other transport is for peers already paired.
const PAIRING_ON_TCP: &str =
    "pairing runs over tcp:// only; other transports serve peers that are already paired";

fn parse_id(text: &str) -> Result<PeerId, String> {
    text.parse::<PeerId>()
        .map_err(|_| "that is not a peer id: use the form `bohio peer list` prints".to_owned())
}

/// `bohio serve`.
pub(super) fn serve(args: &ServeArgs, environment: &Environment) -> Result<(), String> {
    let (config, _) = load(args.config.config.as_deref(), environment)?;
    if args.stdio {
        if !args.listen.is_empty() {
            return Err("--stdio serves standard input and output alone: drop --listen".to_owned());
        }
        // Standard output is the link: nothing else is written to it, and [[listen]] entries are not bound.
        let server = Server::new(&config, environment).map_err(|error| error.to_string())?;
        tracing::info!(host = %server.host(), "serving standard input and output");
        return server.serve_stdio().map_err(|error| error.to_string());
    }
    let mut entries = config.listen.clone();
    entries.extend(args.listen.iter().map(|endpoint| ListenConfig {
        endpoint: endpoint.clone(),
        ..ListenConfig::default()
    }));
    serve_endpoints(&config, environment, &entries, &AtomicBool::new(false))
}

/// Binds every entry, prints what is listening, and serves them until `stop` is set. A host started
/// by the Windows service manager stops through the flag; `bohio serve` runs until it is killed.
pub(super) fn serve_endpoints(
    config: &Config,
    environment: &Environment,
    entries: &[ListenConfig],
    stop: &AtomicBool,
) -> Result<(), String> {
    if entries.is_empty() {
        return Err(
            "nothing to listen on: add a [[listen]] entry to bohio.cfg or pass --listen".to_owned(),
        );
    }
    let server = Server::new(config, environment).map_err(|error| error.to_string())?;
    let settings = config.link.settings();
    let mut listeners = Vec::new();
    for entry in entries {
        let (listening, bound) =
            Listening::bind(entry, &settings).map_err(|error| error.to_string())?;
        say!("listening {bound}");
        listeners.push(listening);
    }
    say!("host {}", server.host());
    tracing::info!(host = %server.host(), "serving");
    thread::scope(|scope| {
        let handles: Vec<_> = listeners
            .iter()
            .map(|listening| scope.spawn(|| server.serve_listening(listening, stop)))
            .collect();
        for handle in handles {
            if let Ok(Err(error)) = handle.join() {
                tracing::error!("a listener failed: {error}");
            }
        }
    });
    Ok(())
}

/// `bohio id`.
pub(super) fn id(args: &StatusArgs, environment: &Environment) -> Result<(), String> {
    let (_, data) = source(&args.config, environment)?;
    let identity = data.identity().map_err(|error| error.to_string())?;
    say!("{}", identity.peer_id());
    Ok(())
}

/// `bohio status`.
pub(super) fn status(args: &StatusArgs, environment: &Environment) -> Result<(), String> {
    let (config, data) = source(&args.config, environment)?;
    let identity = data.identity().map_err(|error| error.to_string())?;
    let listing = peer::list(&data).map_err(|error| error.to_string())?;
    say!("id: {}", identity.peer_id());
    say!("method: {}", identity.method());
    say!("data_dir: {}", data.dir().display());
    say!("peers: {}", data.policy_path().display());
    say!("accepted: {}", listing.accepted.len());
    say!("waiting: {}", listing.waiting.len());
    for listen in &config.listen {
        let state = match Endpoint::parse(&listen.endpoint) {
            Ok(endpoint) => answers(&endpoint),
            Err(_) => "not served by this build",
        };
        say!("listen: {} {state}", listen.endpoint);
    }
    Ok(())
}

/// Whether something answers at `endpoint`, by connecting where that is safe to do and, for a
/// serial line, whether the device is there.
fn answers(endpoint: &Endpoint) -> &'static str {
    let connects = |address: &std::net::SocketAddr| {
        TcpStream::connect_timeout(address, Duration::from_secs(1)).is_ok()
    };
    let reachable = |yes: bool| if yes { "reachable" } else { "not reachable" };
    match endpoint {
        Endpoint::Tcp(address) => reachable(connects(address)),
        Endpoint::WebSocket(url) => reachable(
            url.socket_addrs(|| None)
                .is_ok_and(|addresses| addresses.iter().any(connects)),
        ),
        Endpoint::Pipe(name) => reachable(
            coqui_transport_pipe::Stream::builder()
                .name(name)
                .timeout(Duration::from_secs(1))
                .build()
                .is_ok(),
        ),
        Endpoint::Serial { path, .. } => {
            if cfg!(unix) {
                if std::path::Path::new(path).exists() {
                    "device present"
                } else {
                    "device missing"
                }
            } else {
                "device not checked"
            }
        }
        Endpoint::Command(_) => "not a listening endpoint",
        _ => "not served by this build",
    }
}

/// `bohio peer`.
pub(super) fn peer(args: &PeerArgs, environment: &Environment) -> Result<(), String> {
    match &args.action {
        PeerAction::List(list) => peer_list(list, environment),
        PeerAction::Approve(approve) => peer_approve(approve, environment),
        PeerAction::Revoke(revoke) => peer_revoke(revoke, environment),
    }
}

fn describe(grant: &Grant) -> String {
    let names: Vec<&str> = grant.permissions().map(Permission::name).collect();
    if names.is_empty() {
        return "nothing".to_owned();
    }
    let mut text = names.join(",");
    if !grant.fs_roots().is_empty() {
        let roots: Vec<String> = grant
            .fs_roots()
            .iter()
            .map(|root| root.display().to_string())
            .collect();
        text.push_str(&format!(" fs_roots={}", roots.join(",")));
    }
    if grant.restricts_programs() {
        text.push_str(" programs=listed");
        if !grant.env_names().is_empty() {
            text.push_str(&format!(" env={}", grant.env_names().join(",")));
        }
    }
    text
}

fn peer_list(args: &StatusArgs, environment: &Environment) -> Result<(), String> {
    let (_, data) = source(&args.config, environment)?;
    let listing = peer::list(&data).map_err(|error| error.to_string())?;
    for accepted in &listing.accepted {
        say!(
            "accepted {} allow: {}",
            accepted.id,
            describe(&accepted.grant)
        );
    }
    for waiting in &listing.waiting {
        let source = waiting
            .source
            .map_or_else(|| "unknown".to_owned(), |source| source.ip().to_string());
        say!("waiting {} from {source}", waiting.id);
    }
    for stray in &listing.orphaned {
        say!("grant-only {stray} (not accepted: it holds nothing)");
    }
    if listing.accepted.is_empty() && listing.waiting.is_empty() {
        say!("no peers");
    }
    Ok(())
}

fn peer_approve(args: &ApproveArgs, environment: &Environment) -> Result<(), String> {
    let (_, data) = source(&args.config, environment)?;
    let id = parse_id(&args.id)?;
    let mut permissions = Vec::new();
    for name in &args.allow {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        permissions.push(Permission::from_name(name).ok_or_else(|| {
            format!("`{name}` is not a permission: info, fs.read, fs.write, exec, shell, admin")
        })?);
    }
    let grant = (!permissions.is_empty() || !args.fs_root.is_empty() || !args.program.is_empty())
        .then(|| {
            Grant::allowing(permissions)
                .with_fs_roots(args.fs_root.iter().cloned())
                .with_programs(args.program.iter().cloned())
                .with_env(args.env.iter().cloned())
        });
    peer::approve(&data, &id, grant).map_err(|error| error.to_string())?;
    let held = data
        .policies()
        .map_err(|error| error.to_string())?
        .grant_for(&id.to_string());
    say!("approved {id} allow: {}", describe(&held));
    Ok(())
}

fn peer_revoke(args: &RevokeArgs, environment: &Environment) -> Result<(), String> {
    let (_, data) = source(&args.config, environment)?;
    let id = parse_id(&args.id)?;
    peer::revoke(&data, &id).map_err(|error| error.to_string())?;
    say!("revoked {id}");
    Ok(())
}

/// `bohio pair`.
pub(super) fn pair(args: &PairArgs, environment: &Environment) -> Result<(), String> {
    let (config, data) = source(&args.config, environment)?;
    let timeout = match &args.timeout {
        Some(text) => {
            Span::parse(text)
                .ok_or_else(|| {
                    "a time is a number of seconds or a unit of ms, s, m or h".to_owned()
                })?
                .0
        }
        None => Duration::from_secs(300),
    };
    let Endpoint::Tcp(address) =
        Endpoint::parse(args.listen.as_deref().unwrap_or("tcp://127.0.0.1:0"))?
    else {
        return Err(PAIRING_ON_TCP.to_owned());
    };
    let listener = TcpListener::bind(address)
        .map_err(|error| format!("cannot listen on {address}: {error}"))?;
    let bound = listener.local_addr().map_err(|error| error.to_string())?;
    let identity = data.identity().map_err(|error| error.to_string())?;
    // The PIN is for the person who reads it out, so it goes to standard output and nowhere else.
    let joined = peer::pair(&data, &listener, &config.link.settings(), timeout, |pin| {
        say!("pairing on {}", Endpoint::Tcp(bound));
        say!("host {}", identity.peer_id());
        say!("pin {pin}");
    })
    .map_err(|error| error.to_string())?;
    say!("paired {joined}");
    say!("it holds nothing until `bohio peer approve {joined} --allow ...`");
    Ok(())
}

/// `bohio join`.
pub(super) fn join(args: &JoinArgs, environment: &Environment) -> Result<(), String> {
    let (config, data) = source(&args.config, environment)?;
    let Endpoint::Tcp(address) = Endpoint::parse(&args.endpoint)? else {
        return Err(PAIRING_ON_TCP.to_owned());
    };
    let text = match &args.pin {
        Some(pin) => pin.clone(),
        None => {
            let mut line = String::new();
            std::io::stdin()
                .lock()
                .read_line(&mut line)
                .map_err(|error| format!("cannot read the PIN: {error}"))?;
            line.trim().to_owned()
        }
    };
    let pin = Pin::parse(&CodeAlphabet::pin(), &text)
        .map_err(|_| "that is not a PIN: it is six digits".to_owned())?;
    let identity: Identity = data.identity().map_err(|error| error.to_string())?;
    let store = data.store().map_err(|error| error.to_string())?;
    let settings = config.link.settings();
    let stream = TcpStream::connect_timeout(&address, settings.proof_timeout)
        .map_err(|error| format!("cannot reach {address}: {error}"))?;
    if stream.local_addr().ok() == stream.peer_addr().ok() {
        return Err(format!("nothing is listening on {address}"));
    }
    let channel = socket(stream, &settings).map_err(|error| error.to_string())?;
    let joined = pairing::join(&channel, &identity, pin, &store, settings.proof_timeout);
    channel.close();
    let paired = joined.map_err(|error| error.to_string())?;
    say!("paired with {}", paired.peer.id());
    say!(
        "ask the host to `bohio peer approve {}` this peer",
        identity.peer_id()
    );
    Ok(())
}
