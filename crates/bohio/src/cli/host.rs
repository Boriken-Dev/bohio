//! The commands that run or manage this host: `serve`, `status`, `id`, `peer`, `pair`, `join`.

use std::io::{BufRead, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use bohio_proto::local::PairReply;
use coqui_link::auth::Identity;
use coqui_link::pairing;
use coqui_peer::pair::{CodeAlphabet, Pin};
use coqui_peer::{KeyMaterial, PeerId};
use coqui_transport::Channel;

use super::access::{self, Guarded};
use super::commands::{Found, found, source};
use super::remote::{certificate_refused, tls_trust};
use super::{
    ApproveArgs, JoinArgs, PairArgs, PeerAction, PeerArgs, RevokeArgs, ServeArgs, StatusArgs,
};
use crate::config::{Config, DialConfig, Environment, ListenConfig};
use crate::forward::{ConnectRule, ListenRule};
use crate::link::{
    Grants, KeptDirection, KeptState, Keys, KeysError, LogEvents, MemoryGrants, NodeEvent,
    NodeEvents,
};
use crate::listen::Listening;
use crate::local::{ANOTHER_ACCOUNT, Local, LocalLink, Server};
use crate::node::{Endpoint, Node};
use crate::peer::{self, Data, LayeredPeers};
use crate::policy::{Grant, Permission};

/// Standard output, flushed: a person or a test reads these lines as they happen.
macro_rules! say {
    ($($arg:tt)*) => {{
        let mut out = std::io::stdout().lock();
        let _ = writeln!(out, $($arg)*);
        let _ = out.flush();
    }};
}

fn parse_id(text: &str) -> Result<PeerId, String> {
    text.parse::<PeerId>()
        .map_err(|_| "that is not a peer id: use the form `bohio peer list` prints".to_owned())
}

/// The node `config` describes, with the identity of its data directory and the peers of the layers `watch`.
fn node(
    found: &Found,
    watch: Vec<PathBuf>,
    environment: &Environment,
) -> Result<(Node, Arc<LayeredPeers>), String> {
    let config = found.config();
    let data = Data::new(config.data_dir(environment));
    let identity = data.identity().map_err(|error| error.to_string())?;
    let create = found.create.clone().filter(|file| watch.contains(file));
    let peers = Arc::new(
        LayeredPeers::new(watch, &data)
            .map_err(|error| error.to_string())?
            .with_create(create),
    );
    let node = Node::new(
        node_settings(config, environment),
        identity,
        peers.clone(),
        peers.clone(),
    )
    .map_err(|error| error.to_string())?
    .with_pairing(data.store().map_err(|error| error.to_string())?)
    .map_err(|error| error.to_string())?;
    let node = node.with_path(environment.path.clone().unwrap_or_default());
    #[cfg(feature = "notify")]
    let node = node.with_notifier(Arc::new(crate::gui::DesktopNotifier::new()));
    Ok((node, peers))
}

/// A node's events as log lines, and the forwards it keeps as lines on standard output: a person or a test reads where each
/// one listens, and that it came back, as it happens.
struct Printed;

impl NodeEvents for Printed {
    fn event(&self, event: &NodeEvent) {
        if let NodeEvent::Kept {
            peer,
            direction,
            spec,
            state,
        } = event
        {
            let (word, to) = match direction {
                KeptDirection::Local => ("local", spec.split_once('=').map_or("", |(_, to)| to)),
                _ => ("remote", spec.split_once('=').map_or("", |(_, to)| to)),
            };
            match state {
                KeptState::Up { address } => say!("{word} {address} -> {to} peer {peer}"),
                KeptState::Down => say!("{word} down {spec} peer {peer}"),
                KeptState::Failed { reason, retry_in } => {
                    say!("{word} failed {spec} peer {peer}: {reason} (retry in {retry_in:?})");
                }
            }
        }
        LogEvents.event(event);
    }
}

/// `bohio serve`.
pub(super) fn serve(args: &ServeArgs, environment: &Environment) -> Result<(), String> {
    let found = found(&args.config.config)?;
    let config = found.config();
    if args.stdio {
        if !args.listen.is_empty() {
            return Err("--stdio serves standard input and output alone: drop --listen".to_owned());
        }
        // Standard output is the link: nothing else is written to it, and [[listen]] entries are not bound.
        let watch = guard(&found, environment)?;
        let (server, _) = node(&found, watch, environment)?;
        tracing::info!(host = %server.host(), "serving standard input and output");
        return server.serve_stdio().map_err(|error| error.to_string());
    }
    let mut entries = config.listen.clone();
    entries.extend(args.listen.iter().map(|endpoint| ListenConfig {
        endpoint: endpoint.clone(),
        ..ListenConfig::default()
    }));
    serve_endpoints(&found, environment, &entries, &AtomicBool::new(false))
}

/// Refuses to serve from a layer or a data directory another user may change, unless `[service]
/// check_access` is false: then what was found is logged and the host serves. A layer or directory another
/// account owns is refused either way. Answers the layers to read the peers from while serving.
fn guard(found: &Found, environment: &Environment) -> Result<Vec<PathBuf>, String> {
    let config = found.config();
    let mut problems = Vec::new();
    for file in &found.layered.files {
        problems.extend(access::layer_problems(file));
    }
    access::decide(&problems, config.service.check_access)?;
    // The layers are the operator's, so the directory they name is made (as the node would make it) and checked.
    let data = config.data_dir(environment);
    let _ = std::fs::create_dir_all(&data);
    let problems: Vec<_> = access::check(&data, Guarded::DataDir)
        .err()
        .into_iter()
        .collect();
    access::decide(&problems, config.service.check_access)?;
    Ok(watched(found))
}

/// The layers whose peers are read again while serving: each layer read at start, and each that does not exist
/// yet in a place no other account may change. One another account could make is left out until a restart
/// checks it, since a layer made while serving is read unchecked.
fn watched(found: &Found) -> Vec<PathBuf> {
    let check_access = found.config().service.check_access;
    found
        .watch
        .iter()
        .filter(|file| {
            if found.layered.files.contains(file) {
                return true;
            }
            match access::may_appear(file) {
                Ok(()) => true,
                Err(problem) if problem.kind == access::Kind::Unknown && !check_access => {
                    tracing::warn!(
                        "{} is read if it is made, as `check_access` is false: {}",
                        file.display(),
                        problem.text
                    );
                    true
                }
                Err(problem) => {
                    tracing::info!(
                        "{} is not read until a restart if it is made: {}",
                        file.display(),
                        problem.text
                    );
                    false
                }
            }
        })
        .cloned()
        .collect()
}

/// The peers a node dials, from the merged configuration: each id, endpoint text and dial settings. The text
/// is read again at every attempt, so nothing is resolved here.
pub(super) fn dial_targets(config: &Config) -> Result<Vec<(PeerId, String, DialConfig)>, String> {
    config
        .peer
        .dials()
        .map(|(name, endpoint, dial)| Ok((parse_id(name)?, endpoint.to_owned(), dial.clone())))
        .collect()
}

/// Binds every entry, prints what is listening, dials every peer whose table has an `endpoint`, and
/// serves them all until `stop` is set. A host started by the Windows service manager stops through the
/// flag; `bohio serve` runs until it is killed. The peers dialed are read once, here: a restart applies a
/// new endpoint.
pub(super) fn serve_endpoints(
    found: &Found,
    environment: &Environment,
    entries: &[ListenConfig],
    stop: &AtomicBool,
) -> Result<(), String> {
    let config = found.config();
    let dials = dial_targets(config)?;
    if entries.is_empty() && dials.is_empty() {
        return Err(
            "nothing to listen on or dial: add a [[listen]] entry or a peer with an `endpoint` to bohio.cfg, or pass --listen"
                .to_owned(),
        );
    }
    let watch = guard(found, environment)?;
    let (server, peers) = node(found, watch, environment)?;
    let server = server.with_events(Arc::new(Printed));
    let settings = config.link.settings();
    let mut listeners = Vec::new();
    for entry in entries {
        let (listening, bound) =
            Listening::bind(entry, &settings).map_err(|error| error.to_string())?;
        say!("listening {bound}");
        if let Some(hash) = listening.certificate_hash() {
            say!("certificate {hash} for {bound}");
        }
        listeners.push(listening);
    }
    let local = if config.control.is_enabled() {
        match Local::bind(&config.control, &server.host()) {
            Ok(local) => {
                say!("control {}", local.endpoint());
                Some(local)
            }
            Err(error) => {
                tracing::warn!("the local control endpoint is not served: {error}");
                None
            }
        }
    } else {
        None
    };
    say!("host {}", server.host());
    for (id, endpoint, _) in &dials {
        say!("dialing {endpoint} peer {id}");
    }
    tracing::info!(host = %server.host(), "serving");
    thread::scope(|scope| {
        let handles: Vec<_> = listeners
            .iter()
            .map(|listening| scope.spawn(|| server.serve_listening(listening, stop)))
            .collect();
        if let Some(local) = &local {
            let server = &server;
            scope.spawn(move || {
                if let Err(error) = server.serve_local(local, stop) {
                    tracing::error!("the local control endpoint failed: {error}");
                }
            });
        }
        for (id, endpoint, dial) in &dials {
            let server = &server;
            scope.spawn(move || server.run_dial(id, endpoint, dial, stop));
        }
        {
            let (server, peers) = (&server, &peers);
            scope.spawn(move || server.run_forwards(&**peers, stop));
        }
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
    let (found, data) = source(&args.config, environment)?;
    let identity = data.identity().map_err(|error| error.to_string())?;
    let listing = peer::list(&found.layered.files, &data).map_err(|error| error.to_string())?;
    say!("id: {}", identity.peer_id());
    say!("method: {}", identity.method());
    say!("data_dir: {}", data.dir().display());
    for file in &found.layered.files {
        say!("config: {}", file.display());
    }
    say!("accepted: {}", listing.accepted.len());
    say!("waiting: {}", listing.waiting.len());
    for listen in &found.config().listen {
        let state = match Endpoint::parse(&listen.endpoint) {
            Ok(endpoint) => answers(&endpoint, found.config().client.probe_timeout.0),
            Err(_) => "not served by this build",
        };
        say!("listen: {} {state}", listen.endpoint);
    }
    for (id, endpoint, _) in dial_targets(found.config())? {
        say!("dial: {endpoint} peer {id}");
    }
    for (id, table) in found.config().peer.declared() {
        for entry in table.forward_local() {
            say!("keeps: local {entry} peer {id}");
        }
        for entry in table.forward_remote() {
            say!("keeps: remote {entry} peer {id}");
        }
    }
    Ok(())
}

/// Whether something answers at `endpoint`, by connecting where that is safe to do and, for a
/// serial line, whether the device is there.
fn answers(endpoint: &Endpoint, wait: Duration) -> &'static str {
    let connects =
        |address: &std::net::SocketAddr| TcpStream::connect_timeout(address, wait).is_ok();
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
                .timeout(wait)
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
        Endpoint::WebTransport { .. } => "UDP not checked",
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
    if !grant.forward().connect().is_empty() {
        text.push_str(&format!(" connect={}", grant.forward().connect().join(",")));
    }
    if !grant.forward().listen().is_empty() {
        text.push_str(&format!(" listen={}", grant.forward().listen().join(",")));
    }
    text
}

fn peer_list(args: &StatusArgs, environment: &Environment) -> Result<(), String> {
    let (found, data) = source(&args.config, environment)?;
    let listing = peer::list(&found.layered.files, &data).map_err(|error| error.to_string())?;
    for accepted in &listing.accepted {
        let dialed = accepted
            .endpoint
            .as_ref()
            .map_or_else(String::new, |endpoint| format!(" dial: {endpoint}"));
        say!(
            "accepted {} allow: {}{dialed}",
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
    if listing.accepted.is_empty() && listing.waiting.is_empty() {
        say!("no peers");
    }
    Ok(())
}

fn peer_approve(args: &ApproveArgs, environment: &Environment) -> Result<(), String> {
    let (found, data) = source(&args.config, environment)?;
    let id = parse_id(&args.id)?;
    if let Some(endpoint) = &args.endpoint {
        Endpoint::parse(endpoint).map_err(|reason| format!("--endpoint: {reason}"))?;
    }
    let mut permissions = Vec::new();
    for name in &args.allow {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        permissions.push(Permission::from_name(name).ok_or_else(|| {
            format!(
                "`{name}` is not a permission: info, fs.read, fs.write, exec, shell, admin, forward.connect, gui.view, gui.input, gui.clipboard, forward.listen"
            )
        })?);
    }
    for rule in &args.connect {
        rule.parse::<ConnectRule>()
            .map_err(|error| format!("--connect `{rule}`: {error}"))?;
    }
    for rule in &args.listen {
        rule.parse::<ListenRule>()
            .map_err(|error| format!("--listen `{rule}`: {error}"))?;
    }
    // A rule that the permission does not accompany would allow nothing, so naming rules grants the permission.
    if !args.connect.is_empty() && !permissions.contains(&Permission::ForwardConnect) {
        permissions.push(Permission::ForwardConnect);
    }
    if !args.listen.is_empty() && !permissions.contains(&Permission::ForwardListen) {
        permissions.push(Permission::ForwardListen);
    }
    let grant = (!permissions.is_empty()
        || !args.fs_root.is_empty()
        || !args.program.is_empty()
        || !args.connect.is_empty()
        || !args.listen.is_empty())
    .then(|| {
        Grant::allowing(permissions)
            .with_fs_roots(args.fs_root.iter().cloned())
            .with_programs(args.program.iter().cloned())
            .with_env(args.env.iter().cloned())
            .with_connect(args.connect.iter().cloned())
            .with_listen(args.listen.iter().cloned())
    });
    let approved = peer::approve(
        &found.layered.files,
        found.create.as_deref(),
        &data,
        &id,
        grant.as_ref(),
        args.endpoint.as_deref(),
    )
    .map_err(|error| error.to_string())?;
    let held = crate::config::Layers::peers(&if approved.made {
        let mut files = found.layered.files.clone();
        files.push(approved.file.clone());
        files
    } else {
        found.layered.files.clone()
    })
    .map_err(|error| error.to_string())?
    .grant_for(&id.to_string());
    say!("approved {id} allow: {}", describe(&held));
    say!("wrote {}", approved.file.display());
    Ok(())
}

fn peer_revoke(args: &RevokeArgs, environment: &Environment) -> Result<(), String> {
    let (found, data) = source(&args.config, environment)?;
    let id = parse_id(&args.id)?;
    let revoked =
        peer::revoke(&found.layered.files, &data, &id).map_err(|error| error.to_string())?;
    for file in &revoked.files {
        say!("removed {id} from {}", file.display());
    }
    for file in &revoked.unwritable {
        say!(
            "cannot write {}: it still accepts {id}; ask whoever owns it to remove the table",
            file.display()
        );
    }
    say!("revoked {id}");
    Ok(())
}

/// `bohio pair`: arms the running node, or serves pairing alone on the configured listeners when none runs.
pub(super) fn pair(args: &PairArgs, environment: &Environment) -> Result<(), String> {
    let (found, data) = source(&args.config, environment)?;
    let config = found.config();
    let identity = data.identity().map_err(|error| error.to_string())?;
    if config.control.is_enabled()
        && let Ok(control) = Endpoint::parse(&config.control.endpoint_for(&identity.peer_id()))
    {
        let server = Server::for_control(&config.control);
        match LocalLink::connect_to(&control, &config.link.settings(), server) {
            Ok(link) => return pair_running(&link, &found, &identity.peer_id(), args),
            // A PIN or a "paired" from another account's endpoint would be its own, so nothing is asked of it.
            Err(bohio_client::Error::Link(text)) if text == ANOTHER_ACCOUNT => {
                return Err(format!("{text}: another account bound the name first"));
            }
            Err(_) => {}
        }
    }
    pair_alone(&found, environment, args)
}

/// What `bohio pair` prints about the listeners: the entries as the configuration writes them.
fn say_listeners(entries: &[ListenConfig]) {
    for entry in entries {
        say!("pairing on {}", entry.endpoint);
    }
}

fn pair_running(
    link: &LocalLink,
    found: &Found,
    host: &PeerId,
    args: &PairArgs,
) -> Result<(), String> {
    let pairing = link
        .pair(args.timeout)
        .map_err(|error| format!("the running node cannot arm a PIN: {error}"))?;
    say_listeners(&found.config().listen);
    say!("host {host}");
    // The PIN is for the person who reads it out, so it goes to standard output and nowhere else.
    say!("pin {}", pairing.pin());
    match pairing.wait().map_err(|error| error.to_string())? {
        PairReply::Paired { peer } => {
            say!("paired {peer}");
            say!("it holds nothing until `bohio peer approve {peer} --allow ...`");
            Ok(())
        }
        PairReply::Ended { reason } => Err(format!("nobody paired: {reason}")),
        _ => Err("the node said something this command does not know".to_owned()),
    }
}

/// The keys of a node that only pairs: it accepts nobody, so no proven peer is served while the PIN is armed, and
/// a joiner is recorded as `inner` records it.
struct PairOnly(LayeredPeers);

impl Keys for PairOnly {
    fn is_accepted(&self, _peer: &PeerId) -> bool {
        false
    }

    fn paired(&self, peer: &PeerId, key: &KeyMaterial) -> Result<(), KeysError> {
        self.0.paired(peer, key)
    }
}

fn pair_alone(found: &Found, environment: &Environment, args: &PairArgs) -> Result<(), String> {
    let config = found.config();
    let mut entries = config.listen.clone();
    match (&args.listen, entries.is_empty()) {
        (Some(_), false) => {
            return Err(
                "--listen is for a configuration with no [[listen]] entry: pairing is served on the entries it has"
                    .to_owned(),
            );
        }
        (Some(endpoint), true) => entries.push(ListenConfig {
            endpoint: endpoint.clone(),
            ..ListenConfig::default()
        }),
        (None, true) => {
            return Err(
                "no node runs and there is nothing to listen on: add a [[listen]] entry to bohio.cfg or pass --listen"
                    .to_owned(),
            );
        }
        (None, false) => {}
    }
    let data = Data::new(config.data_dir(environment));
    let identity = data.identity().map_err(|error| error.to_string())?;
    let watch = watched(found);
    let create = found.create.clone().filter(|file| watch.contains(file));
    let peers = LayeredPeers::new(watch, &data)
        .map_err(|error| error.to_string())?
        .with_create(create);
    let node = Node::new(
        config.node(),
        identity,
        Arc::new(PairOnly(peers)),
        Arc::new(MemoryGrants::new()) as Arc<dyn Grants>,
    )
    .map_err(|error| error.to_string())?
    .with_pairing(data.store().map_err(|error| error.to_string())?)
    .map_err(|error| error.to_string())?;
    let settings = config.link.settings();
    let mut listeners = Vec::new();
    for entry in &entries {
        let (listening, bound) = Listening::bind(entry, &settings).map_err(|error| {
            format!("{error}; if `bohio serve` runs here without a [control] endpoint, `bohio pair` cannot reach it")
        })?;
        say!("pairing on {bound}");
        if let Some(hash) = listening.certificate_hash() {
            say!("certificate {hash} for {bound}");
        }
        listeners.push(listening);
    }
    let armed = node
        .arm_pairing(args.timeout)
        .map_err(|error| error.to_string())?;
    say!("host {}", node.host());
    // The PIN is for the person who reads it out, so it goes to standard output and nowhere else.
    say!("pin {}", armed.pin());
    let stop = AtomicBool::new(false);
    let end = thread::scope(|scope| {
        for listening in &listeners {
            let (node, stop) = (&node, &stop);
            scope.spawn(move || {
                if let Err(error) = node.serve_listening(listening, stop) {
                    tracing::error!("a listener failed: {error}");
                }
            });
        }
        let end = loop {
            if let Some(end) = armed.wait(settings.poll_interval) {
                break end;
            }
        };
        stop.store(true, Ordering::SeqCst);
        end
    });
    match end {
        crate::pair::PairEnd::Paired(peer) => {
            say!("paired {peer}");
            say!("it holds nothing until `bohio peer approve {peer} --allow ...`");
            Ok(())
        }
        other => Err(format!("nobody paired: {other}")),
    }
}

/// `bohio join`.
pub(super) fn join(args: &JoinArgs, environment: &Environment) -> Result<(), String> {
    let (found, data) = source(&args.config, environment)?;
    let config = found.config();
    let endpoint = Endpoint::parse(&args.endpoint)?;
    let endpoint = match &args.fallback {
        Some(text) => endpoint.with_fallback(Endpoint::parse(text)?)?,
        None => endpoint,
    };
    let endpoint = args.mtu.apply(endpoint)?;
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
    let pin = Pin::parse(&CodeAlphabet::new("0123456789", text.len()), &text)
        .map_err(|_| "that is not a PIN: it is digits only".to_owned())?;
    let identity: Identity = data.identity().map_err(|error| error.to_string())?;
    let store = data.store().map_err(|error| error.to_string())?;
    let settings = config.link.settings();
    let trust = tls_trust(&endpoint, &args.ca, &args.cert_hash, args.clock_skew)?;
    let channel = endpoint
        .dial_with(settings.proof_timeout, &settings, &trust)
        .map_err(|error| match error {
            bohio_client::Error::Transport(coqui_transport::Failure::Certificate) => {
                certificate_refused(&endpoint, &trust)
            }
            other => format!("cannot reach {endpoint}: {other}"),
        })?;
    let joined = pairing::join_shared(&channel, &identity, pin, &store, settings.proof_timeout);
    channel.close();
    let paired = joined.map_err(|error| error.to_string())?;
    let host = paired.peer.id();
    let approved = peer::approve(
        &found.layered.files,
        found.create.as_deref(),
        &data,
        &host,
        None,
        None,
    )
    .map_err(|error| error.to_string())?;
    say!("paired with {host}");
    say!("wrote {}", approved.file.display());
    say!(
        "ask the host to `bohio peer approve {}` this peer",
        identity.peer_id()
    );
    Ok(())
}

/// The settings a node serves with: the file's, and prepared reads kept in `prepared` in the data directory unless
/// the file names another place.
pub(super) fn node_settings(
    config: &crate::config::Config,
    environment: &Environment,
) -> crate::config::NodeConfig {
    let mut settings = config.node();
    settings
        .compression
        .prepare_dir
        .get_or_insert_with(|| config.data_dir(environment).join("prepared"));
    settings
}

#[cfg(test)]
mod tests {
    use super::access::{Kind, Problem, decide};

    fn problem(kind: Kind) -> Problem {
        Problem {
            kind,
            text: "the data directory is open to every user".to_owned(),
        }
    }

    #[test]
    fn an_access_problem_is_an_error_until_the_operator_turns_the_check_off() {
        assert!(decide(&[problem(Kind::Open)], true).is_err());
        assert!(decide(&[problem(Kind::Open)], false).is_ok());
        assert!(decide(&[problem(Kind::Unknown)], false).is_ok());
        assert!(decide(&[], true).is_ok());
    }

    /// Whoever owns a layer wrote what it says, `check_access = false` included, so a layer another account owns
    /// cannot turn its own check off.
    #[test]
    fn a_path_another_account_owns_is_refused_even_when_the_check_is_off() {
        assert!(decide(&[problem(Kind::Foreign)], false).is_err());
        assert!(decide(&[problem(Kind::Open), problem(Kind::Foreign)], false).is_err());
    }
}
