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
use bohio_client::forward::ForwardSettings;
use bohio_client::fs::{PutOptions, ReadOptions, Reader, put, resolve_auto};
use bohio_client::hash::hash_many;
use bohio_client::keep::{Backoff, Kept, Links, Reconnecting, Relink};
use bohio_client::link::Settings;
use bohio_client::shell::{Event as ShellEvent, Shell};
use bohio_client::sync::{Compare, DeltaMode, Moved, SyncOptions};
use bohio_client::{Error, Open};
use bohio_proto::compress::{Codec, Compression};
use bohio_proto::exec::{Request, Stdin};
use bohio_proto::forward::{Bind, Target};
use bohio_proto::fs::{WriteMode, WriteRequest};
use bohio_proto::hash::{Algorithm, HashItem};
use bohio_proto::session::{OnFull, SessionRequest};
use bohio_proto::shell::{Request as ShellRequest, ShellKind};
use bohio_proto::signal::Signal;
use coqui_link::auth::{Proof, Trust};
use coqui_peer::PeerId;

use super::commands::{print_info, source};
use super::{
    Format, KeepArgs, OnFullArg, RemoteAction, RemoteArgs, RemoteAttachArgs, RemoteConnectArgs,
    RemoteEndArgs, RemoteExecArgs, RemoteForwardArgs, RemoteHashArgs, RemoteListenArgs,
    RemoteSessionsArgs, RemoteShellArgs, RemoteWriteArgs, SessionArgs,
};
use crate::config::{DialConfig, Environment, Layers};
use crate::forward::{BadSpec, LocalSpec, RemoteKeep, RemoteSpec, SocksSpec, keep_remote};
use crate::link::KeptState;
use crate::local::{ANOTHER_ACCOUNT, Server, Via};
use crate::node::Endpoint;

/// The waits and sizes of one `bohio remote` run: `[client]`, with the flags laid over it, and `[link] poll_interval`.
#[derive(Debug, Clone, Copy)]
pub(super) struct Client {
    /// The wait for a question's first answer.
    pub(super) reply: Duration,
    /// The wait for one step of a transfer; `None` is no limit.
    pub(super) transfer: Option<Duration>,
    /// The wait for a `gui` action.
    pub(super) gui: Duration,
    /// Bytes of standard input read at a time.
    pub(super) input_chunk: usize,
    /// Bytes of a local file read at a time.
    pub(super) read_size: usize,
    /// Chunks of standard input held ahead of a connection.
    pub(super) input_queue: usize,
    /// How often a loop that waits for input or a stream looks again.
    pub(super) poll: Duration,
}

impl Client {
    pub(super) fn new(config: &crate::config::Config, args: &RemoteArgs) -> Self {
        let client = &config.client;
        Self {
            reply: args.reply_timeout.unwrap_or(client.reply_timeout.0),
            transfer: args.transfer_timeout.map_or_else(
                || client.transfer(),
                |wait| (!wait.is_zero()).then_some(wait),
            ),
            gui: client.gui_timeout.0,
            input_chunk: client.input_chunk,
            read_size: client.read_size,
            input_queue: client.input_queue,
            poll: config.link.poll_interval.0.max(Duration::from_millis(1)),
        }
    }
}

/// What a person is told when the host will not do what was asked.
pub(super) fn explain(error: &Error) -> String {
    match error {
        Error::Remote(body) => format!("the host refused: {}", body.message),
        Error::Transport(coqui_transport::Failure::Permission) => {
            "the host refused or ended the stream: this peer may not do that, or its access was revoked".to_owned()
        }
        other => other.to_string(),
    }
}

fn direct_plan(args: &RemoteArgs, environment: &Environment) -> Result<Plan, String> {
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
    let endpoint = args.mtu.apply(endpoint)?;
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
    Ok(Plan::Direct {
        endpoint,
        proof: Arc::new(proof),
        settings,
        trust,
    })
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
    let plan = match &args.via {
        Some(id) => via_plan(id, args, environment)?,
        None => direct_plan(args, environment)?,
    };
    let link: Arc<dyn Open> = plan.dial()?;
    // A command that stays up makes the link again when it ends, unless `--once`.
    let persist = match &args.action {
        RemoteAction::Connect(connect) => Some(&connect.persist),
        RemoteAction::Forward(forward) => Some(&forward.persist),
        RemoteAction::Listen(listen) => Some(&listen.persist),
        _ => None,
    };
    let found = source(&args.config, environment)?.0;
    let settings = found.config().forward.client();
    let client = Client::new(found.config(), args);
    let kept = match persist {
        Some(persist) => keeping(
            plan,
            &link,
            persist,
            backoff(args, persist, environment)?,
            settings.options.listen_poll,
        ),
        None => None,
    };
    match &args.action {
        RemoteAction::Info(info) => {
            let control = Control::open(&*link).map_err(|error| explain(&error))?;
            let found = control
                .info(Some(client.reply))
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
        RemoteAction::Exec(exec) => exec_command(&*link, exec, &client),
        RemoteAction::Read(read) => {
            let mut request = bohio_proto::fs::ReadRequest::whole(read.path.clone());
            request.compression = wanted(read.compress.as_deref(), read.window_log)?;
            request.prepare = read.prepare;
            // A prepared read is silent while the host compresses: the host's progress frames keep the wait alive.
            let reader =
                Reader::start_with(&*link, &request, Some(client.reply), ReadOptions::default())
                    .map_err(|error| explain(&error))?;
            let mut out = std::io::stdout().lock();
            reader
                .copy_to(&mut out, None)
                .map_err(|error| explain(&error))?;
            out.flush().map_err(|error| error.to_string())?;
            Ok(ExitCode::SUCCESS)
        }
        RemoteAction::Write(write) => write_command(&*link, write, &client),
        RemoteAction::Hash(hash) => hash_command(&*link, hash),
        RemoteAction::Put(put) => {
            let options = sync_options(&*link, &put.sync, true, &client)?;
            let moved = bohio_client::sync::put(&*link, &put.local, &put.remote, &options)
                .map_err(|error| explain(&error))?;
            report_moved(&moved)
        }
        RemoteAction::Get(get) => {
            let options = sync_options(&*link, &get.sync, false, &client)?;
            let moved = bohio_client::sync::get(&*link, &get.remote, &get.local, &options)
                .map_err(|error| explain(&error))?;
            report_moved(&moved)
        }
        RemoteAction::Shell(shell) => shell_command(&*link, shell, &client),
        RemoteAction::Attach(attach) => attach_command(&*link, attach, &client),
        RemoteAction::Sessions(sessions) => sessions_command(&*link, sessions, &client),
        RemoteAction::End(end) => end_command(&*link, end, &client),
        RemoteAction::Connect(connect) => {
            let open = relinked(&link, kept.as_ref(), &settings);
            connect_command(&*open, connect, &settings)
        }
        RemoteAction::Gui(gui) => super::gui::run(&*link, gui, &client),
        RemoteAction::Forward(forward) => forward_command(
            &link,
            kept.as_ref(),
            &backoff(args, &forward.persist, environment)?,
            forward,
            &settings,
        ),
        RemoteAction::Listen(listen) => listen_command(
            &link,
            kept.as_ref(),
            &backoff(args, &listen.persist, environment)?,
            listen,
            &settings,
            &client,
        ),
    }
}

/// What opens a command's streams: the link, or with a kept link whichever is up when the stream is opened.
fn relinked(
    link: &Arc<dyn Open>,
    kept: Option<&Arc<Reconnecting>>,
    settings: &ForwardSettings,
) -> Arc<dyn Open> {
    match kept {
        Some(kept) => Arc::new(Relink::new(
            Arc::clone(kept) as Arc<dyn Links>,
            settings.wait_for_link,
            settings.options.listen_poll,
        )),
        None => Arc::clone(link),
    }
}

/// How the link to the host is made again after it ends: what `connect` and `connect_via` did, kept.
enum Plan {
    /// A link of its own to an endpoint.
    Direct {
        endpoint: Endpoint,
        proof: Arc<Proof>,
        settings: Settings,
        trust: ServerTrust,
    },
    /// The node's link to a peer, through the node's local control endpoint.
    Via {
        endpoint: Endpoint,
        settings: Settings,
        server: Server,
        peer: PeerId,
        /// How long the node is given to list its links (`[client] links_wait`).
        wait: Duration,
    },
}

impl Plan {
    fn dial(&self) -> Result<Arc<dyn Open>, String> {
        match self {
            Self::Direct {
                endpoint,
                proof,
                settings,
                trust,
            } => Connection::connect_with(endpoint, settings.proof_timeout, proof, settings, trust)
                .map(|connection| Arc::new(connection) as Arc<dyn Open>)
                .map_err(|error| match error {
                    Error::Transport(coqui_transport::Failure::Certificate) => {
                        certificate_refused(endpoint, trust)
                    }
                    Error::Transport(_) | Error::Closed => format!(
                        "the link to {endpoint} ended before it was proven: the host may not be listening, \
                         may not trust this key yet (it waits for `bohio peer approve`), or this side does \
                         not trust the host (`bohio join`, or --server ID)"
                    ),
                    other => other.to_string(),
                }),
            Self::Via {
                endpoint,
                settings,
                server,
                peer,
                wait,
            } => {
                let via = Via::connect_to(endpoint, peer, settings, *server)
                    .map_err(|error| unreachable(&error))?;
                let listed = via
                    .links(*wait)
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
                Ok(Arc::new(via))
            }
        }
    }
}

/// The waits between attempts to make the link again: the `[dial]` table of the peer the command reaches when a configured one
/// is named (`--server`, `--via`, or the endpoint of a `[peer."<id>"]` table), else the dial defaults, then the flags.
fn backoff(
    args: &RemoteArgs,
    keep: &KeepArgs,
    environment: &Environment,
) -> Result<Backoff, String> {
    let (found, _) = source(&args.config, environment)?;
    let peers = &found.config().peer;
    let named = args
        .via
        .as_deref()
        .or(args.server.as_deref())
        .and_then(|id| peers.table(id))
        .or_else(|| {
            let endpoint = args.endpoint.as_deref()?;
            peers
                .dials()
                .find(|(_, text, _)| *text == endpoint)
                .and_then(|(id, _, _)| peers.table(id))
        });
    let mut backoff = named.map_or_else(
        || DialConfig::default().backoff(),
        |table| table.dial().backoff(),
    );
    let span = |text: &str, flag: &str| {
        crate::config::Span::parse(text)
            .map(|span| span.0)
            .ok_or_else(|| format!("{flag} is a time such as `1s` or `500ms`"))
    };
    if let Some(text) = &keep.retry_min {
        backoff.min = span(text, "--retry-min")?;
    }
    if let Some(text) = &keep.retry_max {
        backoff.max = span(text, "--retry-max")?;
    }
    if let Some(factor) = keep.retry_factor {
        backoff.factor = factor;
    }
    if let Some(jitter) = keep.retry_jitter {
        backoff.jitter = jitter;
    }
    if backoff.min.is_zero() || backoff.max < backoff.min || backoff.factor < 1.0 {
        return Err(
            "--retry-min is not zero, --retry-max at least --retry-min and --retry-factor at least 1"
                .to_owned(),
        );
    }
    if !(0.0..=1.0).contains(&backoff.jitter) {
        return Err("--retry-jitter is from 0 to 1".to_owned());
    }
    Ok(backoff)
}

/// The link a command that stays up keeps: it is made again each time it ends, and each change is told on standard error.
/// `None` with `--once`: the first link is the only one.
fn keeping(
    plan: Plan,
    first: &Arc<dyn Open>,
    keep: &KeepArgs,
    backoff: Backoff,
    poll: Duration,
) -> Option<Arc<Reconnecting>> {
    if keep.once {
        return None;
    }
    let report: Box<dyn Fn(&Kept) + Send + Sync> = Box::new(|change| match change {
        Kept::Lost => eprintln!("bohio: the link to the host was lost; making a new one"),
        Kept::Failed { reason, retry_in } => {
            eprintln!("bohio: no link yet ({reason}); trying again in {retry_in:?}");
        }
        Kept::Restored => eprintln!("bohio: the link to the host is back"),
        _ => {}
    });
    Some(Arc::new(Reconnecting::start(
        Arc::clone(first),
        Box::new(move || plan.dial()),
        backoff,
        poll,
        report,
    )))
}

/// `bohio remote ... connect`: the host connects to a target and its bytes meet standard input and output.
fn connect_command(
    link: &dyn Open,
    args: &RemoteConnectArgs,
    settings: &ForwardSettings,
) -> Result<ExitCode, String> {
    let target: Target = args
        .target
        .parse()
        .map_err(|error: bohio_proto::forward::BadTarget| error.to_string())?;
    let tunnel = bohio_client::forward::connect(link, &target, settings)
        .map_err(|error| explain_forward(&error))?;
    if args.check {
        match tunnel.address() {
            Some(address) => println!("connected {address}"),
            None => println!("connected"),
        }
        return Ok(ExitCode::SUCCESS);
    }
    let mut out = std::io::stdout().lock();
    tunnel
        .bridge(std::io::stdin(), &mut out)
        .map_err(|error| explain_forward(&error))?;
    Ok(ExitCode::SUCCESS)
}

/// What a person is told when a connect or a local forward fails: the host's reason, or that the grant does not cover the
/// target.
fn explain_forward(error: &Error) -> String {
    explain_with(error, "forward.connect", "connect")
}

/// As [`explain_forward`], for what a remote forward or `listen` asked of the host.
fn explain_listen(error: &Error) -> String {
    explain_with(error, "forward.listen", "listen")
}

fn explain_with(error: &Error, permission: &str, verb: &str) -> String {
    match error {
        Error::Remote(body) => format!(
            "the host did not {verb} ({:?}): {}",
            body.code, body.message
        ),
        Error::Transport(coqui_transport::Failure::Permission) => format!(
            "the host refused or ended the forward: this peer holds no `{permission}` rule for that, or its access was revoked"
        ),
        Error::Closed => "the host ended the stream before the other end did".to_owned(),
        other => other.to_string(),
    }
}

/// `bohio remote ... forward`: `-L` listens here and `-R` has the host listen. With a kept link they run until interrupted, the
/// listeners staying up while the link is made again; with `--once`, until the link ends (status 1).
fn forward_command(
    link: &Arc<dyn Open>,
    kept: Option<&Arc<Reconnecting>>,
    backoff: &Backoff,
    args: &RemoteForwardArgs,
    settings: &ForwardSettings,
) -> Result<ExitCode, String> {
    let mut bound = Vec::new();
    for text in &args.local {
        let spec: LocalSpec = text
            .parse()
            .map_err(|error: BadSpec| format!("-L `{text}`: {error}"))?;
        let (listener, shown) = spec
            .bind(settings)
            .map_err(|error| format!("-L `{text}`: {error}"))?;
        if let Some(address) = spec.non_default() {
            println!("non-default: bind {address}");
        }
        bound.push((spec, listener, shown));
    }
    let mut remote = Vec::new();
    for text in &args.remote {
        let spec: RemoteSpec = text
            .parse()
            .map_err(|error: BadSpec| format!("-R `{text}`: {error}"))?;
        if let Some(host) = spec.non_default() {
            println!("non-default: bind {host}");
        }
        remote.push(spec);
    }
    for (spec, _, shown) in &bound {
        println!("local {shown} -> {}", spec.target());
    }
    let _ = std::io::stdout().flush();
    let stop = std::sync::atomic::AtomicBool::new(false);
    let report = |error: &Error| {
        eprintln!(
            "bohio: a forwarded connection failed: {}",
            explain_forward(error)
        )
    };
    let reporter: Arc<dyn Fn(&Error) + Send + Sync> = Arc::new(|error: &Error| {
        eprintln!(
            "bohio: a forwarded connection failed: {}",
            explain_listen(error)
        );
    });
    // What opens a local listener's connections: the link, or whichever link is up when a connection arrives.
    let open = relinked(link, kept, settings);
    let refused: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    let mut forwards = Vec::new();
    for spec in &remote {
        let forward = bohio_client::forward::RemoteForward::open(
            Arc::clone(link),
            spec.bind(),
            spec.local(),
            settings,
            Arc::clone(&reporter),
        )
        .map_err(|error| {
            format!(
                "-R `{}={}`: {}",
                spec.bind(),
                spec.local(),
                explain_listen(&error)
            )
        })?;
        forwards.push(forward);
    }
    let open: &dyn Open = &*open;
    let lost = thread::scope(|scope| {
        // With a kept link each remote forward is asked for again after a loss, for the same port; with `--once` a watcher
        // ends the command when one ends.
        let mut watcher = None;
        let mut asking = Vec::new();
        if let Some(kept) = kept {
            let (stop, reporter, refused) = (&stop, &reporter, &refused);
            for (spec, forward) in remote.iter().zip(forwards.drain(..)) {
                let local = spec.local().clone();
                asking.push(scope.spawn(move || {
                    let tell = |state: KeptState| match state {
                        KeptState::Up { address } => {
                            println!("remote {address} -> {local}");
                            let _ = std::io::stdout().flush();
                        }
                        KeptState::Down => {
                            eprintln!("bohio: a remote forward ended; asking for it again");
                        }
                        KeptState::Failed { reason, retry_in } => {
                            eprintln!(
                                "bohio: a remote forward could not be made again ({reason}); trying again in {retry_in:?}"
                            );
                        }
                    };
                    let ended = keep_remote(
                        &RemoteKeep {
                            links: &**kept,
                            settings,
                            backoff,
                            poll: settings.options.listen_poll,
                            stop,
                            retry_closed: false,
                            tell: &tell,
                            report: Arc::clone(reporter),
                        },
                        spec,
                        Some(forward),
                    );
                    if ended.is_some() {
                        // The host said no with the link up (a revoke): that ends the command, as it always did.
                        *refused.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(
                            "the host ended a remote listener: its access was revoked or it failed"
                                .to_owned(),
                        );
                        stop.store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                }));
            }
        } else {
            for (spec, forward) in remote.iter().zip(&forwards) {
                println!("remote {} -> {}", forward.address(), spec.local());
            }
            watcher = (!forwards.is_empty()).then(|| {
                let (stop, forwards) = (&stop, &forwards);
                scope.spawn(move || {
                    let mut failure = None;
                    while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                        if let Some(ended) = forwards.iter().find_map(|forward| forward.ended()) {
                            failure = Some(match ended {
                                bohio_client::forward::Ended::Closed => {
                                    "the host ended a remote listener: its access was revoked or it failed"
                                        .to_owned()
                                }
                                _ => "the link to the host ended".to_owned(),
                            });
                            stop.store(true, std::sync::atomic::Ordering::SeqCst);
                            break;
                        }
    let mut adjusted = *settings;
    if let Some(wait) = args.socks_timeout {
        if wait.is_zero() {
            return Err("--socks-timeout is not zero".to_owned());
        }
        adjusted.socks_timeout = wait;
    }
    let settings = &adjusted;
                        thread::sleep(settings.options.listen_poll);
                    }
                    failure
                })
            });
        }
        let _ = std::io::stdout().flush();
        let mut handles: Vec<_> = bound
            .iter()
            .map(|(spec, listener, _)| {
                let (stop, report) = (&stop, &report);
                scope.spawn(move || {
                    let ended = listener.serve(open, spec.target(), settings, stop, report);
    let mut socks = Vec::new();
    for text in &args.dynamic {
        let spec: SocksSpec = text
            .parse()
            .map_err(|error: BadSpec| format!("-D `{text}`: {error}"))?;
        let (listener, shown) = spec
            .bind()
            .map_err(|error| format!("-D `{text}`: {error}"))?;
        if let Some(address) = spec.non_default() {
            println!("non-default: bind {address}");
        }
        socks.push((listener, shown));
    }
                    // One listener ending ends them all: the link is gone, or this one could not go on.
                    stop.store(true, std::sync::atomic::Ordering::SeqCst);
                    ended
                })
            })
            .collect();
        let mut failure = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(bohio_client::forward::Ended::Stopped)) => {}
                Ok(Ok(_)) => failure = Some("the link to the host ended".to_owned()),
                Ok(Err(error)) => failure = Some(format!("a listener failed: {error}")),
                Err(_) => failure = Some("a listener failed".to_owned()),
    for (_, shown) in &socks {
        println!("socks5 {shown}");
    }
            }
        }
        for handle in asking {
            let _ = handle.join();
        }
        if let Some(watcher) = watcher {
            // With no local listener nothing has ended yet: the watcher is what the command waits for.
            if !(bound.is_empty() && socks.is_empty()) {
                stop.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            if let Ok(Some(reason)) = watcher.join() {
                failure = Some(reason);
            }
        }
        if let Some(reason) = refused
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            failure = Some(reason);
        }
        failure
    });
    drop(forwards);
    lost.map_or(Ok(ExitCode::SUCCESS), Err)
}

/// The compression a flag asks for: `ALG[:LEVEL]` and the window, `None` when the flag is absent.
fn wanted(flag: Option<&str>, window_log: Option<u32>) -> Result<Option<Compression>, String> {
    let Some(text) = flag else {
        return Ok(None);
    };
    let mut compression =
        Compression::parse(text).map_err(|reason| format!("--compress: {reason}"))?;
    if let Codec::Other(name) = &compression.algorithm {
        return Err(format!(
            "--compress: `{name}` is not an algorithm; the names are zstd, lz4, deflate, none and auto"
        ));
    }
    compression.window_log = window_log;
    Ok(Some(compression))
}

/// The algorithm a client names for what it sends: `auto` is the host's default, read from its `info`.
fn named(
    link: &dyn Open,
    compression: Compression,
    client: &Client,
) -> Result<Compression, String> {
    if compression.algorithm != Codec::Auto {
        return Ok(compression);
    }
    let control = Control::open(link).map_err(|error| explain(&error))?;
    let info = control
        .info(Some(client.reply))
        .map_err(|error| explain(&error))?;
    resolve_auto(&info, &compression).ok_or_else(|| {
        "the host reports no default compression: name an algorithm, such as --compress zstd:3"
            .to_owned()
    })
}

/// What `put` and `get` ask of the sync functions. A put names its algorithm itself (`auto` is read from the host's
/// `info`); a get may leave `auto` to the host.
fn sync_options(
    link: &dyn Open,
    args: &super::SyncArgs,
    sends: bool,
    client: &Client,
) -> Result<SyncOptions, String> {
    let compression = match wanted(args.compress.as_deref(), args.window_log)? {
        Some(compression) if compression.algorithm == Codec::None => None,
        Some(compression) if sends => Some(named(link, compression, client)?),
        other => other,
    };
    Ok(SyncOptions {
        delta: match args.delta {
            super::DeltaChoice::Auto => DeltaMode::Auto,
            super::DeltaChoice::Always => DeltaMode::Always,
            super::DeltaChoice::Never => DeltaMode::Never,
        },
        compare: args.compare.map(|how| match how {
            super::CompareChoice::Meta => Compare::Meta,
            super::CompareChoice::Hash => Compare::Hash(Algorithm::Blake3),
            super::CompareChoice::Blocks => Compare::Blocks,
        }),
        compression,
        block_size: args.block_size,
        timeout: client.transfer,
        read_size: client.read_size,
        ..SyncOptions::default()
    })
}

/// The one line `put` and `get` print.
fn report_moved(moved: &Moved) -> Result<ExitCode, String> {
    let line = if moved.skipped {
        format!("sent 0 of {} (unchanged)", moved.size)
    } else {
        format!(
            "sent {} of {} (copied {}, literal {})",
            moved.wire_bytes, moved.size, moved.copied_bytes, moved.literal_bytes
        )
    };
    let mut out = std::io::stdout().lock();
    writeln!(out, "{line}")
        .and_then(|()| out.flush())
        .map_err(|error| error.to_string())?;
    Ok(ExitCode::SUCCESS)
}

/// `bohio remote ... write`: standard input to a file of the host.
fn write_command(
    link: &dyn Open,
    args: &RemoteWriteArgs,
    client: &Client,
) -> Result<ExitCode, String> {
    let mut request = WriteRequest::replace(args.path.clone());
    request.parents = args.parents;
    if args.append {
        request.mode = WriteMode::Append;
    } else if args.create {
        request.mode = WriteMode::Create;
    }
        for (listener, _) in &socks {
            let (stop, report) = (&stop, &report);
            handles.push(scope.spawn(move || {
                let ended = bohio_client::socks::serve(open, listener, settings, stop, report);
                stop.store(true, std::sync::atomic::Ordering::SeqCst);
                ended
            }));
        }
    let compression = match wanted(args.compress.as_deref(), args.window_log)? {
        Some(compression) if compression.algorithm == Codec::None => None,
        Some(compression) => Some(named(link, compression, client)?),
        None => None,
    };
    let options = PutOptions {
        compression,
        prepare: args.prepare,
        ..PutOptions::default()
    };
    let done = put(
        link,
        &request,
        &mut std::io::stdin().lock(),
        &options,
        client.transfer,
    )
    .map_err(|error| explain(&error))?;
    eprintln!("bohio: wrote {} bytes", done.written);
    Ok(ExitCode::SUCCESS)
}

/// What `listen --keep` reads standard input through: one thread reads it for good and each connection takes what comes next,
/// so a connection that ended does not leave a reader behind that takes the next one input.
struct SharedInput {
    state: Arc<std::sync::Mutex<InputState>>,
    poll: Duration,
}

/// The channel the reading thread feeds, and what a connection that ended gave back unread.
struct InputState {
    chunks: std::sync::mpsc::Receiver<Option<Vec<u8>>>,
    given_back: std::collections::VecDeque<Vec<u8>>,
    ended: bool,
}

impl SharedInput {
    fn start(client: &Client) -> Self {
        let chunk = client.input_chunk;
        let (sender, chunks) = std::sync::mpsc::sync_channel(client.input_queue);
        thread::spawn(move || {
            let mut input = std::io::stdin().lock();
            let mut buffer = vec![0_u8; chunk.max(1)];
            loop {
                let chunk = match input.read(&mut buffer) {
                    Ok(0) | Err(_) => None,
                    Ok(read) => Some(buffer[..read].to_vec()),
                };
                let last = chunk.is_none();
                if sender.send(chunk).is_err() || last {
                    return;
                }
            }
        });
        Self {
            state: Arc::new(std::sync::Mutex::new(InputState {
                chunks,
                given_back: std::collections::VecDeque::new(),
                ended: false,
            })),
            poll: client.poll,
        }
    }

    /// A reader for one connection; it stops reading when `retired` is set.
    fn reader(&self, retired: &Arc<std::sync::atomic::AtomicBool>) -> ConnectionInput {
        ConnectionInput {
            state: Arc::clone(&self.state),
            retired: Arc::clone(retired),
            rest: Vec::new(),
            poll: self.poll,
        }
    }
}

struct ConnectionInput {
    state: Arc<std::sync::Mutex<InputState>>,
    retired: Arc<std::sync::atomic::AtomicBool>,
    rest: Vec<u8>,
    poll: Duration,
}

impl Read for ConnectionInput {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        use std::sync::atomic::Ordering::SeqCst;
        loop {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // A connection that ended takes nothing more, and what it would have taken waits for the next one.
            if self.retired.load(SeqCst) {
                if !self.rest.is_empty() {
                    state.given_back.push_front(std::mem::take(&mut self.rest));
                }
                return Ok(0);
            }
            if !self.rest.is_empty() {
                let count = self.rest.len().min(buffer.len());
                buffer[..count].copy_from_slice(&self.rest[..count]);
                self.rest.drain(..count);
                return Ok(count);
            }
            if let Some(chunk) = state.given_back.pop_front() {
                self.rest = chunk;
                continue;
            }
            if state.ended {
                return Ok(0);
            }
            match state.chunks.recv_timeout(self.poll) {
                Ok(Some(chunk)) if self.retired.load(SeqCst) => {
                    state.given_back.push_back(chunk);
                    return Ok(0);
                }
                Ok(Some(chunk)) => self.rest = chunk,
                Ok(None) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    state.ended = true;
                    return Ok(0);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

/// What a command that waits for a link does when there is none: waits, saying so once.
fn next_link(kept: &Reconnecting, poll: Duration) -> Arc<dyn Open> {
    let mut told = false;
    loop {
        if let Some(link) = kept.current() {
            return link;
        }
        if !told {
            eprintln!("bohio: waiting for the link to the host");
            told = true;
        }
        thread::sleep(poll);
    }
}

/// Whether `error` says the link ended: what a kept link is made again for.
const fn link_lost(error: &Error) -> bool {
    matches!(
        error,
        Error::Closed | Error::Transport(coqui_transport::Failure::PathLost) | Error::Link(_)
    )
}

/// `bohio remote ... listen`: the host binds, and an arriving connection meets standard input and output. With a kept link, a
/// link that ends is made again and the host is asked to listen again, for the same port.
fn listen_command(
    link: &Arc<dyn Open>,
    kept: Option<&Arc<Reconnecting>>,
    backoff: &Backoff,
    args: &RemoteListenArgs,
    settings: &ForwardSettings,
    client: &Client,
) -> Result<ExitCode, String> {
    let mut bind: Bind = args
        .bind
        .parse()
        .map_err(|error: bohio_proto::forward::BadTarget| error.to_string())?;
    let poll = settings.options.listen_poll;
    let shared = args.keep.then(|| SharedInput::start(client));
    let mut current = Arc::clone(link);
    let mut first = true;
    let mut failures = 0_u32;
    let mut jitter = bohio_client::keep::Jitter::new();
    loop {
        let listener = match bohio_client::forward::listen(&*current, &bind, None, settings) {
            Ok(listener) => listener,
            Err(error) => match kept {
                // After the first, a refusal is asked again as a lost link is: the host may not have noticed the old listener
                // is gone, and holds its port until it does.
                Some(kept) if !first => {
                    failures = failures.saturating_add(1);
                    let wait = backoff.wait(failures, jitter.sample());
                    if !link_lost(&error) {
                        eprintln!(
                            "bohio: could not listen again ({}); trying again in {wait:?}",
                            explain_listen(&error)
                        );
                    }
                    thread::sleep(wait);
                    current = next_link(kept, poll);
                    continue;
                }
                _ => return Err(explain_listen(&error)),
            },
        };
        first = false;
        failures = 0;
        bind = crate::forward::same_port(&bind, listener.address());
        // On standard error: standard output is the connection's bytes.
        eprintln!("listening {}", listener.address());
        let _ = std::io::stdout().flush();
        loop {
            let arrival = match listener.next_arrival(Some(poll)) {
                Ok(Some(arrival)) => arrival,
                Ok(None) if current.is_up() => continue,
                Ok(None) => break,
                Err(error) if kept.is_some() && link_lost(&error) => break,
                Err(error) => return Err(explain_listen(&error)),
            };
            let tunnel = match listener.accept(&*current, &arrival, settings) {
                Ok(tunnel) => tunnel,
                Err(error) if kept.is_some() && link_lost(&error) => break,
                Err(error) if args.keep => {
                    eprintln!(
                        "bohio: an arrival could not be claimed: {}",
                        explain_listen(&error)
                    );
                    continue;
                }
                Err(error) => return Err(explain_listen(&error)),
            };
            let mut out = std::io::stdout().lock();
            let retired = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let result = match &shared {
                Some(shared) => tunnel.bridge(shared.reader(&retired), &mut out),
                None => tunnel.bridge(std::io::stdin(), &mut out),
            };
            retired.store(true, std::sync::atomic::Ordering::SeqCst);
            drop(out);
            match result {
                Ok(()) if !args.keep => return Ok(ExitCode::SUCCESS),
                Ok(()) => {}
                Err(error) if !args.keep => return Err(explain_listen(&error)),
                Err(error) => eprintln!(
                    "bohio: a connection ended badly: {}",
                    explain_listen(&error)
                ),
            }
            if !current.is_up() {
                break;
            }
        }
        let Some(kept) = kept else {
            return Err("the link to the host ended".to_owned());
        };
        drop(listener);
        eprintln!("bohio: the link to the host ended; listening again when it is back");
        current = next_link(kept, poll);
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

/// Turns each Ctrl-C of this process into `send(interrupt)`, and one within `kill_after` of the one before into
/// `send(kill)`. Only the first handler of a process is installed; a later call changes nothing.
fn on_ctrl_c(send: impl Fn(Signal) + Send + 'static, kill_after: Duration) {
    let last = std::sync::Mutex::new(None::<std::time::Instant>);
    let _ = ctrlc::set_handler(move || {
        let now = std::time::Instant::now();
        let mut last = last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let second = last.is_some_and(|before| now.duration_since(before) <= kill_after);
        *last = Some(now);
        send(if second {
            Signal::Kill
        } else {
            Signal::Interrupt
        });
    });
}

/// Says on standard error that the host could not deliver a signal this command sent for the person's Ctrl-C.
fn report_undelivered(answer: &bohio_proto::signal::Signalled) {
    if !answer.delivered {
        eprintln!(
            "bohio: the host did not deliver {}: {}",
            answer.signal,
            answer.message.as_deref().unwrap_or("it cannot here")
        );
    }
}

fn exec_command(
    link: &dyn Open,
    args: &RemoteExecArgs,
    client: &Client,
) -> Result<ExitCode, String> {
    if args.pty {
        let mut request = ShellRequest::argv(args.argv.iter().cloned());
        request.cwd.clone_from(&args.cwd);
        request.timeout_ms = args.timeout_ms;
        for pair in &args.env {
            let (name, value) = pair
                .split_once('=')
                .ok_or_else(|| "--env is NAME=VALUE".to_owned())?;
            request.env.set.insert(name.to_owned(), value.to_owned());
        }
        request.session = wanted_session(&args.session);
        return terminal_command(link, request, args.kill_after, client);
    }
    let mut request = Request::argv(args.argv.iter().cloned());
    request.session = wanted_session(&args.session);
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
    if let Some(compression) = wanted(args.compress.as_deref(), None)?
        && compression.algorithm != Codec::None
    {
        request.output_compression = Some(compression.clone());
        if args.stdin {
            request.stdin_compression = Some(named(link, compression, client)?);
        }
    }
    let exec = Arc::new(Exec::start(link, &request).map_err(|error| explain(&error))?);
    if args.session.detach {
        return detached_exec(&exec, client);
    }
    drive_exec(exec, args.stdin, args.kill_after, client)
}

/// The session a command line asks for, when it asks for one.
fn wanted_session(args: &SessionArgs) -> Option<SessionRequest> {
    args.detach.then(|| SessionRequest {
        name: args.name.clone(),
        scrollback: args.scrollback,
        on_full: args.on_full.map(|policy| match policy {
            OnFullArg::Pause => OnFull::Pause,
            OnFullArg::DropOldest => OnFull::DropOldest,
        }),
        linger_ms: args
            .linger
            .map(|linger| u64::try_from(linger.as_millis()).unwrap_or(u64::MAX)),
        detach: true,
    })
}

/// Waits for the host to say the session started, prints its id and lets the stream go: the program keeps running.
fn detached_exec(exec: &Exec, client: &Client) -> Result<ExitCode, String> {
    loop {
        match exec.next(Some(client.reply)) {
            Ok(Some(Event::Started { .. })) => break,
            Ok(Some(_)) => {}
            Ok(None) => return Err("the stream ended before the session started".to_owned()),
            Err(error) => return Err(explain(&error)),
        }
    }
    let id = exec
        .session()
        .ok_or_else(|| "the host started the program but made no session".to_owned())?;
    println!("{id}");
    Ok(ExitCode::SUCCESS)
}

/// Shows an exec's output as it comes, sends standard input when asked, and exits with the program's status.
fn drive_exec(
    exec: Arc<Exec>,
    stdin: bool,
    kill_after: Duration,
    client: &Client,
) -> Result<ExitCode, String> {
    {
        // Ctrl-C here is an interrupt to the program, and a second one soon after ends its tree.
        let exec = Arc::clone(&exec);
        on_ctrl_c(
            move |signal| {
                let _ = exec.signal(signal);
            },
            kill_after,
        );
    }
    if stdin {
        // Detached: it may be blocked reading this process's input when the program ends.
        let exec = Arc::clone(&exec);
        let chunk = client.input_chunk.max(1);
        thread::spawn(move || {
            let mut buffer = vec![0_u8; chunk];
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
            Ok(Some(Event::Signalled(answer))) => report_undelivered(&answer),
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

fn shell_command(
    link: &dyn Open,
    args: &RemoteShellArgs,
    client: &Client,
) -> Result<ExitCode, String> {
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
    terminal_command(link, request, args.kill_after, client)
}

/// Runs `request` on a terminal of the host and bridges it to this one, exiting with the program's status.
fn terminal_command(
    link: &dyn Open,
    request: ShellRequest,
    kill_after: Duration,
    client: &Client,
) -> Result<ExitCode, String> {
    let mut request = request;
    let interactive = std::io::stdin().is_terminal();
    let size = if interactive { local_size() } else { (80, 24) };
    request.cols = size.0;
    request.rows = size.1;
    let detach = request.session.is_some();
    let shell = Arc::new(
        Shell::start(link, &request, Some(client.reply)).map_err(|error| explain(&error))?,
    );
    if detach {
        let id = shell
            .session()
            .ok_or_else(|| "the host started the shell but made no session".to_owned())?;
        println!("{id}");
        return Ok(ExitCode::SUCCESS);
    }
    drive_terminal(&shell, interactive, size, kill_after, client)
}

/// Bridges a terminal session of the host to this one until the program ends, and returns its status.
fn drive_terminal(
    shell: &Arc<Shell>,
    interactive: bool,
    size: (u16, u16),
    kill_after: Duration,
    client: &Client,
) -> Result<ExitCode, String> {
    let mut size = size;
    // Typed keys go to the host as they are, so the local terminal must not echo or edit them, and a typed Ctrl-C is the
    // byte the host's terminal turns into an interrupt. Without a terminal here, Ctrl-C is a signal of this process, which
    // goes to the program as an interrupt.
    let raw = interactive && crossterm::terminal::enable_raw_mode().is_ok();
    if !raw {
        let shell = Arc::clone(shell);
        on_ctrl_c(
            move |signal| {
                let _ = shell.signal(signal);
            },
            kill_after,
        );
    }
    {
        // Detached: it is blocked reading this process's input when the shell ends.
        let shell = Arc::clone(shell);
        let chunk = client.input_chunk.max(1);
        thread::spawn(move || {
            let mut buffer = vec![0_u8; chunk];
            let mut input = std::io::stdin().lock();
            while let Ok(count) = input.read(&mut buffer) {
                if count == 0 || shell.write(&buffer[..count]).is_err() {
                    break;
                }
            }
        });
    }
    let result = loop {
        match shell.next(Some(client.poll)) {
            Ok(Some(ShellEvent::Data(bytes))) => {
                let mut out = std::io::stdout().lock();
                let _ = out.write_all(&bytes);
                let _ = out.flush();
            }
            Ok(Some(ShellEvent::Exit(exit))) => break Ok(exit),
            Ok(Some(ShellEvent::Signalled(answer))) => report_undelivered(&answer),
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
    // A deadline is 124 as it is for `exec`; a program that ended on a signal is 128 and the signal; else its code.
    let code = if exit.timed_out {
        124
    } else if let Some(code) = exit.code {
        code & 0xff
    } else {
        exit.signal.map_or(0, |signal| 128 + signal)
    };
    Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
}

/// `bohio remote ... attach`: the session's kept output from the offset asked for, then live.
fn attach_command(
    link: &dyn Open,
    args: &RemoteAttachArgs,
    client: &Client,
) -> Result<ExitCode, String> {
    let (attachment, attached) = bohio_client::session::attach(link, &args.id, args.from)
        .map_err(|error| explain(&error))?;
    if attached.lost > 0 {
        eprintln!(
            "bohio: the host had forgotten {} bytes before offset {}: the output starts there",
            attached.lost, attached.from
        );
    }
    match attachment {
        bohio_client::session::Attachment::Exec(exec) => {
            drive_exec(Arc::from(exec), args.stdin, args.kill_after, client)
        }
        bohio_client::session::Attachment::Shell(shell) => {
            let interactive = std::io::stdin().is_terminal();
            let size = if interactive { local_size() } else { (80, 24) };
            if interactive {
                // The terminal of a shell that was started elsewhere takes this one's size.
                let _ = shell.resize(size.0, size.1);
            }
            drive_terminal(&Arc::new(shell), interactive, size, args.kill_after, client)
        }
        _ => Err("this version does not know that kind of session".to_owned()),
    }
}

/// `bohio remote ... sessions`.
fn sessions_command(
    link: &dyn Open,
    args: &RemoteSessionsArgs,
    client: &Client,
) -> Result<ExitCode, String> {
    let control = Control::open(link).map_err(|error| explain(&error))?;
    let sessions = control
        .sessions(Some(client.reply))
        .map_err(|error| explain(&error))?;
    match args.format {
        Format::Json => {
            println!(
                "{}",
                serde_json::to_string(&sessions)
                    .map_err(|_| "the sessions cannot be written".to_owned())?
            );
        }
        Format::Text => {
            for session in &sessions {
                let state = match (&session.exit, session.attached) {
                    (Some(exit), _) => format!(
                        "exited {}",
                        exit.code.map_or_else(
                            || exit
                                .signal
                                .map_or("?".to_owned(), |signal| format!("signal {signal}")),
                            |code| code.to_string()
                        )
                    ),
                    (None, true) => "attached".to_owned(),
                    (None, false) => "detached".to_owned(),
                };
                println!(
                    "{}\t{:?}\t{}\tnext {}\tkept {}\t{}\t{}{}",
                    session.id,
                    session.kind,
                    state,
                    session.next,
                    session.buffered,
                    session.name.as_deref().unwrap_or("-"),
                    session.program,
                    session
                        .peer
                        .as_deref()
                        .map_or(String::new(), |peer| format!("\t{peer}")),
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `bohio remote ... end`.
fn end_command(link: &dyn Open, args: &RemoteEndArgs, client: &Client) -> Result<ExitCode, String> {
    let control = Control::open(link).map_err(|error| explain(&error))?;
    control
        .end_session(&args.id, args.signal, Some(client.reply))
        .map_err(|error| explain(&error))?;
    Ok(ExitCode::SUCCESS)
}

/// The node's local control endpoint for the configuration `config` names: the one set, else the default of
/// this machine's key.
pub(super) fn control_endpoint(
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
pub(super) fn unreachable(error: &bohio_client::Error) -> String {
    match error {
        bohio_client::Error::Link(text) if text == ANOTHER_ACCOUNT => format!(
            "{text}: another account bound the name first; stop it, then start `bohio serve` again"
        ),
        _ => NO_NODE.to_owned(),
    }
}

const NO_NODE: &str = "the node's local control endpoint does not answer: `bohio serve` must be running with this configuration and `[control] enabled` not false";

/// The link a running node holds to `id`, through its local control endpoint.
fn via_plan(id: &str, args: &RemoteArgs, environment: &Environment) -> Result<Plan, String> {
    let peer = id
        .parse::<PeerId>()
        .map_err(|_| "that is not a peer id".to_owned())?;
    let (endpoint, settings, server) = control_endpoint(&args.config, environment)?;
    let wait = source(&args.config, environment)?
        .0
        .config()
        .client
        .links_wait
        .0;
    Ok(Plan::Via {
        endpoint,
        settings,
        server,
        peer,
        wait,
    })
}

/// `bohio links`.
pub(super) fn links(args: &super::LinksArgs, environment: &Environment) -> Result<(), String> {
    let wait = match &args.wait {
        Some(text) => {
            crate::config::Span::parse(text)
                .ok_or_else(|| "--wait is a time such as `5s` or `500ms`".to_owned())?
                .0
        }
        None => {
            source(&args.config, environment)?
                .0
                .config()
                .client
                .links_wait
                .0
        }
    };
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
