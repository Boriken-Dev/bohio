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
use bohio_client::parallel::{ParallelOptions, get_parallel, put_parallel, read_parallel};
use bohio_client::profile::Profile;
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
use cabuya::auth::{Proof, Trust};
use coqui_peer::PeerId;

use super::commands::{print_info, source};
use super::output;
use super::{
    KeepArgs, OnFullArg, ParallelArgs, RemoteAction, RemoteArgs, RemoteAttachArgs,
    RemoteConnectArgs, RemoteConsoleArgs, RemoteEndArgs, RemoteExecArgs, RemoteForwardArgs,
    RemoteGlobArgs, RemoteHashArgs, RemoteListenArgs, RemoteReadArgs, RemoteSearchArgs,
    RemoteShellArgs, RemoteSyncArgs, RemoteWalkArgs, RemoteWhichArgs, RemoteWriteArgs, SessionArgs,
    SyncDirection,
};
use crate::config::{DialConfig, Environment, Layers};
use crate::forward::{BadSpec, LocalSpec, RemoteKeep, RemoteSpec, SocksSpec, keep_remote};
use crate::link::KeptState;
use crate::local::{ANOTHER_ACCOUNT, Server, Via};
use crate::mcp::Dialer;
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
    /// The least a power or media call waits.
    pub(super) power_wait: Duration,
    /// How soon a second Ctrl-C ends the program's tree.
    pub(super) kill_after: Duration,
    /// The largest file a tree transfer moves in a bundle, and the most files in one.
    pub(super) bundle: (u64, usize),
    /// How often a loop that waits for input or a stream looks again.
    pub(super) poll: Duration,
    /// The most bytes of a screen rectangle, and the most pixels of a frame, this run holds (`[gui] max_rect_bytes` and
    /// `max_snapshot_pixels`).
    pub(super) screen: (u64, u64),
    /// The profile whose client defaults apply to what a flag leaves unset.
    pub(super) profile: Profile,
    /// Whether the configuration sets `[client] parallel_streams` itself, which then wins over the profile.
    pub(super) streams_set: bool,
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
            power_wait: client.power_wait.0,
            kill_after: client.kill_after.0,
            bundle: (client.sync_bundle_max, client.sync_bundle_items),
            poll: config.link.poll_interval.0.max(Duration::from_millis(1)),
            screen: (config.gui.max_rect_bytes, config.gui.max_snapshot_pixels),
            profile: args.profile.unwrap_or_else(|| {
                client
                    .profile
                    .as_deref()
                    .and_then(Profile::parse)
                    .unwrap_or_default()
            }),
            streams_set: false,
        }
    }
}

/// The code an error is reported with under `--format json`: the host's, or what kind of failure it was.
fn code_of(error: &Error) -> String {
    match error {
        Error::Remote(body) => serde_json::to_value(body.code)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_else(|| "remote".to_owned()),
        Error::Transport(coqui_transport::Failure::Permission) => "denied".to_owned(),
        Error::Timeout => "timeout".to_owned(),
        Error::Closed => "closed".to_owned(),
        Error::Link(_) => "link".to_owned(),
        Error::Transport(_) => "transport".to_owned(),
        Error::Protocol(_) => "protocol".to_owned(),
        Error::Compression(_) => "compression".to_owned(),
        Error::Local(_) => "local".to_owned(),
        _ => "failed".to_owned(),
    }
}

/// What a person is told when the host will not do what was asked.
pub(super) fn explain(error: &Error) -> String {
    output::code(&code_of(error));
    match error {
        Error::Remote(body) => format!("the host refused: {}", body.message),
        Error::Transport(coqui_transport::Failure::Permission) => {
            "the host refused or ended the stream: this peer may not do that, or its access was revoked".to_owned()
        }
        other => other.to_string(),
    }
}

/// What a command line and a named host come to: where the host is, how it is proven and trusted, and the hops to it.
pub(super) struct Reach {
    /// The endpoint text; none when `--command` names the program.
    pub(super) endpoint: Option<String>,
    /// The id the host must prove.
    pub(super) server: Option<String>,
    /// TLS trust of the endpoint this side dials itself.
    pub(super) ca: Vec<std::path::PathBuf>,
    pub(super) cert_hash: Vec<String>,
    /// The endpoint to try when the first does not answer.
    pub(super) fallback: Option<String>,
    /// The hosts to go through, in order, each with the id it must prove.
    pub(super) jumps: Vec<crate::config::Hop>,
}

/// The target `args` name: a word without `://` is a host of `[remote]`, whose settings the flags override; anything else is
/// the endpoint as given.
pub(super) fn target(args: &RemoteArgs, config: &crate::config::Config) -> Result<Reach, String> {
    let flagged_jumps = || {
        args.jump
            .iter()
            .enumerate()
            .map(|(index, endpoint)| crate::config::Hop {
                endpoint: endpoint.clone(),
                server: args.jump_server.get(index).cloned(),
                ca: Vec::new(),
                cert_hash: Vec::new(),
            })
            .collect::<Vec<_>>()
    };
    if args.jump_server.len() > args.jump.len() {
        return Err("--jump-server names more hosts than --jump does".to_owned());
    }
    let Some(text) = args
        .endpoint
        .as_deref()
        .filter(|text| crate::config::Hosts::is_name(text))
    else {
        return Ok(Reach {
            endpoint: args.endpoint.clone(),
            server: args.server.clone(),
            ca: args.ca.clone(),
            cert_hash: args.cert_hash.clone(),
            fallback: args.fallback.clone(),
            jumps: flagged_jumps(),
        });
    };
    let host = config.remote.get(text).ok_or_else(|| {
        output::code("not_found");
        format!(
            "`{text}` is not a host named in [remote], and an endpoint has `://` (see `bohio hosts`)"
        )
    })?;
    let jumps = if args.jump.is_empty() {
        config.remote.hops(text)?
    } else {
        flagged_jumps()
    };
    Ok(Reach {
        endpoint: Some(host.endpoint.clone()),
        server: args.server.clone().or_else(|| host.server.clone()),
        ca: if args.ca.is_empty() {
            host.ca.clone()
        } else {
            args.ca.clone()
        },
        cert_hash: if args.cert_hash.is_empty() {
            host.cert_hash.clone()
        } else {
            args.cert_hash.clone()
        },
        fallback: args.fallback.clone().or_else(|| host.fallback.clone()),
        jumps,
    })
}

fn direct_plan(args: &RemoteArgs, environment: &Environment) -> Result<Plan, String> {
    let (found, data) = source(&args.config, environment)?;
    let config = found.config();
    let target = target(args, config)?;
    let endpoint = match (&target.endpoint, &args.command) {
        (Some(text), None) => Endpoint::parse(text).map_err(|error| error.to_string())?,
        (None, Some(command)) => {
            Endpoint::Command(split_command(command).map_err(|error| error.to_string())?)
        }
        _ => return Err("name the host as an endpoint or with --command, not both".to_owned()),
    };
    let endpoint = match &target.fallback {
        Some(text) => endpoint
            .with_fallback(Endpoint::parse(text).map_err(|error| error.to_string())?)
            .map_err(|error| error.to_string())?,
        None => endpoint,
    };
    let endpoint = args.mtu.apply(endpoint)?;
    let identity = data.identity().map_err(|error| error.to_string())?;
    let proof_for = |server: Option<&str>| -> Result<Arc<Proof>, String> {
        let trust = match server {
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
        Proof::builder(identity.clone())
            .responders(trust)
            .build()
            .map(Arc::new)
            .map_err(|error| error.to_string())
    };
    let proof = proof_for(target.server.as_deref())?;
    let mut jumps = Vec::new();
    for (index, hop_spec) in target.jumps.iter().enumerate() {
        let text = &hop_spec.endpoint;
        let hop = Endpoint::parse(text).map_err(|error| format!("--jump `{text}`: {error}"))?;
        // The first hop is dialed from here, as any endpoint is; each next one is carried by the one before it.
        if index > 0
            && !matches!(
                hop,
                Endpoint::Tcp(_) | Endpoint::Pipe(_) | Endpoint::Command(_)
            )
        {
            return Err(format!(
                "--jump `{text}`: only tcp://, pipe:// and stdio:// hops are carried through another host"
            ));
        }
        let proof = proof_for(hop_spec.server.as_deref())
            .map_err(|error| format!("--jump-server for `{text}`: {error}"))?;
        jumps.push(Jump {
            endpoint: hop,
            proof,
        });
    }
    if !jumps.is_empty()
        && !matches!(
            endpoint,
            Endpoint::Tcp(_) | Endpoint::Pipe(_) | Endpoint::Command(_)
        )
    {
        return Err(format!(
            "the host {endpoint} is reached through jump hosts: only tcp://, pipe:// and stdio:// endpoints are carried through another host"
        ));
    }
    // The receive window of a bulk stream is the profile's unless the file sets it.
    let mut link = config.link.clone();
    if link.bulk_window.is_none() {
        link.bulk_window = Client::new(config, args).profile.defaults().bulk_window;
    }
    let settings = link.settings();
    // TLS trust is for the endpoint this side dials itself: the first hop.
    let dialed = jumps.first().map_or(&endpoint, |jump| &jump.endpoint);
    let (ca, cert_hash) = match (
        target.jumps.first(),
        args.ca.is_empty() && args.cert_hash.is_empty(),
    ) {
        (Some(first), true) => (first.ca.clone(), first.cert_hash.clone()),
        _ => (target.ca.clone(), target.cert_hash.clone()),
    };
    let trust = tls_trust(dialed, &ca, &cert_hash, args.clock_skew)?;
    Ok(Plan::Direct {
        endpoint,
        proof,
        settings,
        trust,
        jumps,
        forward: Box::new(config.forward.client()),
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
        trust.pin(hash).map_err(|error| error.to_string())?;
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
    // A command that stays up makes the link again when it ends, unless `--once`. Its waits are checked before the
    // host is dialled: a wrong invocation is status 2, whatever the link would have said.
    let persist = match &args.action {
        RemoteAction::Connect(connect) => Some(&connect.persist),
        RemoteAction::Forward(forward) => Some(&forward.persist),
        RemoteAction::Listen(listen) => Some(&listen.persist),
        _ => None,
    };
    let waits = match persist
        .map(|persist| backoff(args, persist, environment))
        .transpose()
    {
        Ok(waits) => waits,
        Err(message) => {
            output::fail(&message);
            return Ok(ExitCode::from(2));
        }
    };
    let link: Arc<dyn Open> = plan.dial()?;
    // A command that names a files source asks the host for that one on every file stream and call.
    let named = match &args.action {
        RemoteAction::Read(a) => a.files.source.as_deref(),
        RemoteAction::Write(a) => a.files.source.as_deref(),
        RemoteAction::Hash(a) => a.files.source.as_deref(),
        RemoteAction::Put(a) => a.files.source.as_deref(),
        RemoteAction::Get(a) => a.files.source.as_deref(),
        RemoteAction::Walk(a) => a.files.source.as_deref(),
        RemoteAction::Glob(a) => a.files.source.as_deref(),
        RemoteAction::Search(a) => a.files.source.as_deref(),
        RemoteAction::Sync(a) => a.files.source.as_deref(),
        _ => None,
    };
    let link: Arc<dyn Open> = match named {
        Some(name) => Arc::new(bohio_client::Sourced::shared(link, name)),
        None => link,
    };
    let found = source(&args.config, environment)?.0;
    let settings = found.config().forward.client();
    let mut client = Client::new(found.config(), args);
    client.streams_set = found.layered.sets("client.parallel_streams");
    let spread = match &args.action {
        RemoteAction::Read(read) if !(read.follow || read.follow_name) => {
            Spread::of(&read.parallel, found.config(), &client)
        }
        RemoteAction::Put(put) => Spread::of(&put.parallel, found.config(), &client),
        RemoteAction::Get(get) => Spread::of(&get.parallel, found.config(), &client),
        _ => None,
    };
    let more = match &spread {
        Some(spread) => spread.more_links(&plan)?,
        None => Vec::new(),
    };
    let kept = match (persist, waits) {
        (Some(persist), Some(waits)) => {
            keeping(plan, &link, persist, waits, settings.options.listen_poll)
        }
        _ => None,
    };
    match &args.action {
        RemoteAction::Info => {
            let control = Control::open(&*link).map_err(|error| explain(&error))?;
            let found = control
                .info(Some(client.reply))
                .map_err(|error| explain(&error))?;
            if output::json() {
                println!(
                    "{}",
                    serde_json::to_string(&found)
                        .map_err(|_| "the capabilities cannot be written")?
                );
            } else {
                print_info(&found);
            }
            Ok(ExitCode::SUCCESS)
        }
        RemoteAction::Exec(exec) => exec_command(&*link, exec, &client),
        RemoteAction::Read(read) if read.follow || read.follow_name => {
            follow_command(&*link, read, &client)
        }
        RemoteAction::Read(read) => {
            if read.tail.is_some() || read.interval.is_some() {
                return Err("--tail and --interval go with --follow or -F".to_owned());
            }
            if let Some(spread) = &spread {
                if read.prepare {
                    return Err(
                        "--prepare reads the whole file as one stream: not with --parallel"
                            .to_owned(),
                    );
                }
                let mut options = spread.options(&client);
                options.compression = wanted_or(
                    read.compress.as_deref(),
                    read.window_log,
                    client.profile.defaults().compression,
                )?;
                let links = spread.links(&link, &more);
                let mut out = std::io::stdout().lock();
                read_parallel(&links, &read.path, &mut out, &options)
                    .map_err(|error| explain(&error))?;
                return Ok(done_status());
            }
            let mut request = bohio_proto::fs::ReadRequest::whole(read.path.clone());
            request.compression = wanted_or(
                read.compress.as_deref(),
                read.window_log,
                client.profile.defaults().compression,
            )?;
            request.prepare = read.prepare || prepare_by_size(&*link, &request, &client);
            // A prepared read is silent while the host compresses: the host's progress frames keep the wait alive.
            let reader =
                Reader::start_with(&*link, &request, Some(client.reply), ReadOptions::default())
                    .map_err(|error| explain(&error))?;
            let mut out = std::io::stdout().lock();
            reader
                .copy_to(&mut out, None)
                .map_err(|error| explain(&error))?;
            out.flush().map_err(|error| error.to_string())?;
            Ok(done_status())
        }
        RemoteAction::Write(write) => write_command(&*link, write, &client),
        RemoteAction::Hash(hash) => hash_command(&*link, hash),
        RemoteAction::Walk(walk) => walk_command(&*link, walk, &client),
        RemoteAction::Glob(glob) => glob_command(&*link, glob, &client),
        RemoteAction::Stats => stats_command(&*link, &client),
        RemoteAction::Which(which) => which_command(&*link, which, &client),
        RemoteAction::Search(search) => search_command(&*link, search, &client),
        RemoteAction::Sync(sync) => sync_command(&*link, sync, &client),
        RemoteAction::Put(put) => {
            let options = sync_options(&*link, &put.sync, true, &client)?;
            if let Some(spread) = &spread {
                spread.whole_file(&put.sync)?;
                let mut wanted = spread.options(&client);
                wanted.compression.clone_from(&options.compression);
                let moved = put_parallel(
                    &spread.links(&link, &more),
                    &put.local,
                    &put.remote,
                    &wanted,
                )
                .map_err(|error| explain(&error))?;
                return report_moved(&moved);
            }
            let moved = bohio_client::sync::put(&*link, &put.local, &put.remote, &options)
                .map_err(|error| explain(&error))?;
            report_moved(&moved)
        }
        RemoteAction::Get(get) => {
            let options = sync_options(&*link, &get.sync, false, &client)?;
            if let Some(spread) = &spread {
                spread.whole_file(&get.sync)?;
                let mut wanted = spread.options(&client);
                wanted.compression.clone_from(&options.compression);
                let moved = get_parallel(
                    &spread.links(&link, &more),
                    &get.remote,
                    &get.local,
                    &wanted,
                )
                .map_err(|error| explain(&error))?;
                return report_moved(&moved);
            }
            let moved = bohio_client::sync::get(&*link, &get.remote, &get.local, &options)
                .map_err(|error| explain(&error))?;
            report_moved(&moved)
        }
        RemoteAction::Shell(shell) => shell_command(&*link, shell, &client),
        RemoteAction::Attach(attach) => attach_command(&*link, attach, &client),
        RemoteAction::Console(console) => console_command(&*link, console, &client),
        RemoteAction::Power(power) => power_command(&*link, power, &client),
        RemoteAction::Media(media) => media_command(&*link, media, &client),
        RemoteAction::Sessions => sessions_command(&*link, &client),
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

/// How `get`, `put` and `read` spread one file: the flags over `[client] parallel_*`.
struct Spread {
    streams: usize,
    links: usize,
    range: Option<u64>,
}

impl Spread {
    /// `None` when the file goes the ordinary way: one stream on one link.
    fn of(args: &ParallelArgs, config: &crate::config::Config, run: &Client) -> Option<Self> {
        let client = &config.client;
        let links = args
            .links
            .map_or(client.parallel_links, |links| links as usize);
        // A flag, then the file's own key, then the profile.
        let default = if run.streams_set {
            client.parallel_streams
        } else {
            run.profile.defaults().parallel
        };
        let streams = args
            .parallel
            .map_or(default, |streams| streams as usize)
            .max(links);
        let range = args.range.unwrap_or(client.parallel_range);
        (streams > 1).then_some(Self {
            streams,
            links,
            range: (range > 0).then_some(range),
        })
    }

    /// A transfer that moves the whole file in ranges has no delta and no comparison.
    fn whole_file(&self, sync: &super::SyncArgs) -> Result<(), String> {
        if sync
            .delta
            .is_some_and(|choice| choice != super::DeltaChoice::Auto)
            || sync.compare.is_some()
        {
            return Err(
                "--parallel moves the whole file in ranges: not with --delta or --compare"
                    .to_owned(),
            );
        }
        Ok(())
    }

    fn options(&self, client: &Client) -> ParallelOptions {
        ParallelOptions {
            streams: self.streams,
            range: self.range,
            timeout: client.transfer,
            read_size: client.read_size,
            ..ParallelOptions::default()
        }
    }

    /// The links beyond the first, each dialed with its own proof, all at once. Through `--via` the node has the one
    /// link, so there is no more to dial.
    fn more_links(&self, plan: &Plan) -> Result<Vec<Arc<dyn Open>>, String> {
        if self.links <= 1 {
            return Ok(Vec::new());
        }
        if matches!(plan, Plan::Via { .. }) {
            eprintln!(
                "bohio: --links has no effect through --via: the node keeps one link to the peer"
            );
            return Ok(Vec::new());
        }
        thread::scope(|scope| {
            let dials: Vec<_> = (1..self.links)
                .map(|_| scope.spawn(|| plan.dial()))
                .collect();
            dials
                .into_iter()
                .map(|dial| {
                    dial.join()
                        .unwrap_or_else(|_| Err("a link could not be dialed".to_owned()))
                })
                .collect()
        })
    }

    fn links<'a>(&self, first: &'a Arc<dyn Open>, more: &'a [Arc<dyn Open>]) -> Vec<&'a dyn Open> {
        core::iter::once(&**first)
            .chain(more.iter().map(|link| &**link))
            .collect()
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
        /// The hosts to go through, in order: the first is dialed from here, each next one is carried by the one before.
        jumps: Vec<Jump>,
        /// What the streams that carry a jump wait for.
        forward: Box<ForwardSettings>,
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

/// One host a link goes through: where it is and whom it must prove to be.
struct Jump {
    endpoint: Endpoint,
    proof: Arc<Proof>,
}

/// What a person is told when the link to a host this side dials itself ends or fails before it is proven.
fn dial_failure(endpoint: &Endpoint, error: Error, trust: &ServerTrust) -> String {
    match error {
        Error::Transport(coqui_transport::Failure::Certificate) => {
            certificate_refused(endpoint, trust)
        }
        Error::Transport(_) | Error::Closed => format!(
            "the link to {endpoint} ended before it was proven: the host may not be listening, \
             may not trust this key yet (it waits for `bohio peer approve`), or this side does \
             not trust the host (`bohio join`, or --server ID)"
        ),
        other => other.to_string(),
    }
}

/// The link to `endpoint` through `jumps`: the first is dialed, each next one is carried by the one before, and every hop
/// proves its own host. An error names the hop that failed.
fn dial_through(
    jumps: &[Jump],
    endpoint: &Endpoint,
    proof: &Arc<Proof>,
    settings: &Settings,
    trust: &ServerTrust,
    forward: &ForwardSettings,
) -> Result<Arc<dyn Open>, String> {
    let first = &jumps[0];
    let mut link: Arc<dyn Open> = Arc::new(
        Connection::connect_with(
            &first.endpoint,
            settings.proof_timeout,
            &first.proof,
            settings,
            trust,
        )
        .map_err(|error| {
            format!(
                "jump 1 ({}): {}",
                first.endpoint,
                dial_failure(&first.endpoint, error, trust)
            )
        })?,
    );
    let wanted = jumps[1..]
        .iter()
        .map(|jump| (&jump.endpoint, &jump.proof))
        .chain(core::iter::once((endpoint, proof)));
    for (index, (next, next_proof)) in wanted.enumerate() {
        let through = index + 1;
        let through_name = format!("jump {through} ({})", jumps[index].endpoint);
        let name = if through < jumps.len() {
            format!("jump {} ({next})", through + 1)
        } else {
            format!("the host ({next})")
        };
        let connection = Connection::through(link, next, next_proof, settings, forward).map_err(
            |error| match error {
                Error::Remote(_) | Error::Transport(coqui_transport::Failure::Permission) => {
                    let why = if matches!(next, Endpoint::Command(_)) {
                        explain(&error)
                    } else {
                        explain_forward(&error)
                    };
                    format!("{through_name} would not carry the link to {name}: {why}")
                }
                Error::Link(reason) => format!(
                    "{through_name}, the link to {name}: {reason}: whatever answers there is not the host this side trusts \
                     (--server ID, or --jump-server ID for a jump), or it does not trust this key yet"
                ),
                _ => format!(
                    "the link to {name} through {through_name} ended before it was proven: whatever answers there is \
                     not the host this side trusts (--server ID, or --jump-server ID for a jump), or it does not \
                     trust this key yet"
                ),
            },
        )?;
        link = Arc::new(connection);
    }
    Ok(link)
}

impl Plan {
    fn dial(&self) -> Result<Arc<dyn Open>, String> {
        match self {
            Self::Direct {
                endpoint,
                proof,
                settings,
                trust,
                jumps,
                forward,
            } if !jumps.is_empty() => {
                dial_through(jumps, endpoint, proof, settings, trust, forward)
            }
            Self::Direct {
                endpoint,
                proof,
                settings,
                trust,
                ..
            } => Connection::connect_with(endpoint, settings.proof_timeout, proof, settings, trust)
                .map(|connection| Arc::new(connection) as Arc<dyn Open>)
                .map_err(|error| dial_failure(endpoint, error, trust)),
            Self::Via {
                endpoint,
                settings,
                server,
                peer,
                wait,
            } => {
                let via = Via::connect_to(endpoint, peer, settings, *server)
                    .map_err(|error| unreachable(&error))?;
                let listed = via.links(*wait).map_err(|error| explain(&error))?;
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
    let target = target(args, found.config())?;
    let named = args
        .via
        .as_deref()
        .or(target.server.as_deref())
        .and_then(|id| peers.table(id))
        .or_else(|| {
            let endpoint = target.endpoint.as_deref()?;
            peers
                .dials()
                .find(|(_, text, _)| *text == endpoint)
                .and_then(|(id, _, _)| peers.table(id))
        });
    let mut backoff = named.map_or_else(
        || DialConfig::default().backoff(),
        |table| table.dial().backoff(),
    );
    if let Some(span) = keep.retry_min {
        backoff.min = span;
    }
    if let Some(span) = keep.retry_max {
        backoff.max = span;
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
        Kept::Lost => output::note("the link to the host was lost; making a new one"),
        Kept::Failed { reason, retry_in } => {
            output::note(&format!(
                "no link yet ({reason}); trying again in {retry_in:?}"
            ));
        }
        Kept::Restored => output::note("the link to the host is back"),
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
        if output::json() {
            output::object(
                &serde_json::json!({ "connected": tunnel.address().map(|address| address.to_string()) }),
            );
        } else {
            match tunnel.address() {
                Some(address) => println!("connected {address}"),
                None => println!("connected"),
            }
        }
        return Ok(ExitCode::SUCCESS);
    }
    let mut out = std::io::stdout().lock();
    tunnel
        .bridge(std::io::stdin(), &mut out)
        .map_err(|error| explain_forward(&error))?;
    Ok(done_status())
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
    output::code(&code_of(error));
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
    let mut adjusted = *settings;
    if let Some(wait) = args.socks_timeout {
        if wait.is_zero() {
            return Err("--socks-timeout is not zero".to_owned());
        }
        adjusted.socks_timeout = wait;
    }
    let settings = &adjusted;
    let mut bound = Vec::new();
    for text in &args.local {
        let spec: LocalSpec = text
            .parse()
            .map_err(|error: BadSpec| format!("-L `{text}`: {error}"))?;
        let (listener, shown) = spec
            .bind(settings)
            .map_err(|error| format!("-L `{text}`: {error}"))?;
        if let Some(address) = spec.non_default() {
            non_default(&address.to_string());
        }
        bound.push((spec, listener, shown));
    }
    let mut socks = Vec::new();
    for text in &args.dynamic {
        let spec: SocksSpec = text
            .parse()
            .map_err(|error: BadSpec| format!("-D `{text}`: {error}"))?;
        let (listener, shown) = spec
            .bind()
            .map_err(|error| format!("-D `{text}`: {error}"))?;
        if let Some(address) = spec.non_default() {
            non_default(&address.to_string());
        }
        socks.push((listener, shown));
    }
    let mut remote = Vec::new();
    for text in &args.remote {
        let spec: RemoteSpec = text
            .parse()
            .map_err(|error: BadSpec| format!("-R `{text}`: {error}"))?;
        if let Some(host) = spec.non_default() {
            non_default(host);
        }
        remote.push(spec);
    }
    for (spec, _, shown) in &bound {
        announce(
            "local",
            &shown.to_string(),
            Some(&spec.target().to_string()),
        );
    }
    for (_, shown) in &socks {
        announce("socks5", &shown.to_string(), None);
    }
    let _ = std::io::stdout().flush();
    let stop = std::sync::atomic::AtomicBool::new(false);
    let report = |error: &Error| {
        output::note(&format!(
            "a forwarded connection failed: {}",
            explain_forward(error)
        ));
    };
    let reporter: Arc<dyn Fn(&Error) + Send + Sync> = Arc::new(|error: &Error| {
        output::note(&format!(
            "a forwarded connection failed: {}",
            explain_listen(error)
        ));
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
                            announce("remote", &address.to_string(), Some(&local.to_string()));
                        }
                        KeptState::Down => {
                            output::note("a remote forward ended; asking for it again");
                        }
                        KeptState::Failed { reason, retry_in } => {
                            output::note(&format!(
                                "a remote forward could not be made again ({reason}); trying again in {retry_in:?}"
                            ));
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
                announce("remote", forward.address(), Some(&spec.local().to_string()));
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
                    // One listener ending ends them all: the link is gone, or this one could not go on.
                    stop.store(true, std::sync::atomic::Ordering::SeqCst);
                    ended
                })
            })
            .collect();
        for (listener, _) in &socks {
            let (stop, report) = (&stop, &report);
            handles.push(scope.spawn(move || {
                let ended = bohio_client::socks::serve(open, listener, settings, stop, report);
                stop.store(true, std::sync::atomic::Ordering::SeqCst);
                ended
            }));
        }
        let mut failure = None;
        for handle in handles {
            match handle.join() {
                Ok(Ok(bohio_client::forward::Ended::Stopped)) => {}
                Ok(Ok(_)) => failure = Some("the link to the host ended".to_owned()),
                Ok(Err(error)) => failure = Some(format!("a listener failed: {error}")),
                Err(_) => failure = Some("a listener failed".to_owned()),
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

/// Whether the profile has a compressed read of this file prepared in full first: the profile names a size, the read is
/// compressed, and the file is at least that large. A failed `stat` leaves the read as it was.
fn prepare_by_size(
    link: &dyn Open,
    request: &bohio_proto::fs::ReadRequest,
    client: &Client,
) -> bool {
    let Some(minimum) = client.profile.defaults().prepare_min else {
        return false;
    };
    if !request
        .compression
        .as_ref()
        .is_some_and(Compression::is_requested)
    {
        return false;
    }
    bohio_client::fs::Fs::connect(link)
        .and_then(|fs| {
            fs.with_timeout(Some(client.reply))
                .stat(&request.path, true)
        })
        .is_ok_and(|meta| meta.size >= minimum)
}

/// The compression a flag asks for: `ALG[:LEVEL]` and the window, `None` when the flag is absent.
fn wanted(flag: Option<&str>, window_log: Option<u32>) -> Result<Option<Compression>, String> {
    wanted_or(flag, window_log, None)
}

/// As [`wanted`], and `fallback` (a profile's compression) when the flag is absent.
fn wanted_or(
    flag: Option<&str>,
    window_log: Option<u32>,
    fallback: Option<Compression>,
) -> Result<Option<Compression>, String> {
    let Some(text) = flag else {
        return Ok(fallback);
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
    let defaults = client.profile.defaults();
    let compression = match wanted_or(
        args.compress.as_deref(),
        args.window_log,
        defaults.compression,
    )? {
        Some(compression) if compression.algorithm == Codec::None => None,
        Some(compression) if sends => Some(named(link, compression, client)?),
        other => other,
    };
    Ok(SyncOptions {
        delta: args.delta.map_or(defaults.delta, |choice| match choice {
            super::DeltaChoice::Auto => DeltaMode::Auto,
            super::DeltaChoice::Always => DeltaMode::Always,
            super::DeltaChoice::Never => DeltaMode::Never,
        }),
        compare: args
            .compare
            .map(|how| match how {
                super::CompareChoice::Meta => Compare::Meta,
                super::CompareChoice::Hash => Compare::Hash(Algorithm::Blake3),
                super::CompareChoice::Blocks => Compare::Blocks,
            })
            .or(defaults.compare),
        compression,
        block_size: args.block_size,
        timeout: client.transfer,
        read_size: client.read_size,
        ..SyncOptions::default()
    })
}

/// The one line `put` and `get` print, or under `--format json` one object.
fn report_moved(moved: &Moved) -> Result<ExitCode, String> {
    if output::json() {
        output::object(&serde_json::json!({
            "size": moved.size,
            "literal_bytes": moved.literal_bytes,
            "copied_bytes": moved.copied_bytes,
            "wire_bytes": moved.wire_bytes,
            "delta": moved.delta,
            "skipped": moved.skipped,
        }));
        return Ok(ExitCode::SUCCESS);
    }
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
    let compression = match wanted_or(
        args.compress.as_deref(),
        args.window_log,
        client.profile.defaults().compression,
    )? {
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
    if output::json() {
        output::object(&serde_json::json!({ "written": done.written, "hashes": done.hashes }));
    } else {
        output::note(&format!("wrote {} bytes", done.written));
    }
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
            output::note("waiting for the link to the host");
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
                        output::note(&format!(
                            "could not listen again ({}); trying again in {wait:?}",
                            explain_listen(&error)
                        ));
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
        if output::json() {
            output::status(&serde_json::json!({
                "event": "listening",
                "address": listener.address().to_string(),
            }));
        } else {
            eprintln!("listening {}", listener.address());
        }
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
                    output::note(&format!(
                        "an arrival could not be claimed: {}",
                        explain_listen(&error)
                    ));
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
                Ok(()) if !args.keep => return Ok(done_status()),
                Ok(()) => {}
                Err(error) if !args.keep => return Err(explain_listen(&error)),
                Err(error) => output::note(&format!(
                    "a connection ended badly: {}",
                    explain_listen(&error)
                )),
            }
            if !current.is_up() {
                break;
            }
        }
        let Some(kept) = kept else {
            return Err("the link to the host ended".to_owned());
        };
        drop(listener);
        output::note("the link to the host ended; listening again when it is back");
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
            Ok(digests) if output::json() => {
                output::event(
                    "hash",
                    serde_json::json!({ "path": path, "hashes": digests }),
                );
            }
            Ok(digests) => {
                for algorithm in &algorithms {
                    let hex = digests.get(algorithm).map_or("", String::as_str);
                    let line = if algorithms.len() == 1 {
                        format!("{hex}  {}", output::shown(path))
                    } else {
                        format!("{algorithm} {hex}  {}", output::shown(path))
                    };
                    writeln!(out, "{line}").map_err(|error| error.to_string())?;
                }
            }
            Err(body) => {
                failed = true;
                item_failure(Some(path), &body.message);
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

/// `bohio remote ... walk`: the entries of trees, one line each (or one JSON document each).
fn walk_command(
    link: &dyn Open,
    args: &RemoteWalkArgs,
    client: &Client,
) -> Result<ExitCode, String> {
    let hashes = match &args.hash {
        Some(name) => {
            let algorithm = Algorithm::from_name(name);
            if !algorithm.is_known() {
                return Err(format!("`{name}` is not an algorithm"));
            }
            vec![algorithm]
        }
        None => Vec::new(),
    };
    let compression = match wanted(args.compress.as_deref(), None)? {
        Some(compression) if compression.algorithm == Codec::None => None,
        other => other,
    };
    let options = bohio_client::walk::WalkOptions {
        depth: args.depth,
        follow: args.follow,
        include: args.include.clone(),
        exclude: args.exclude.clone(),
        hashes,
        compression,
        ..bohio_client::walk::WalkOptions::default()
    };
    let roots: Vec<&str> = args.paths.iter().map(String::as_str).collect();
    let iter = bohio_client::walk::walk_with(link, &roots, &options, Some(client.reply))
        .map_err(|error| explain(&error))?;
    let mut out = std::io::stdout().lock();
    let mut failed = false;
    for item in iter {
        match item.map_err(|error| explain(&error))? {
            bohio_client::walk::WalkItem::Entry { root, entry } => {
                let base = roots
                    .get(usize::try_from(root).unwrap_or(usize::MAX))
                    .copied()
                    .unwrap_or("");
                let path = if entry.path.is_empty() {
                    base.to_owned()
                } else {
                    format!("{}/{}", base.trim_end_matches('/'), entry.path)
                };
                if output::json() {
                    output::event(
                        "entry",
                        serde_json::json!({ "root": root, "path": path, "entry": entry }),
                    );
                    continue;
                }
                let kind = match entry.metadata.kind {
                    bohio_proto::fs::FileKind::File => 'f',
                    bohio_proto::fs::FileKind::Directory => 'd',
                    bohio_proto::fs::FileKind::Symlink => 'l',
                    _ => '?',
                };
                let digest = entry
                    .hashes
                    .as_ref()
                    .and_then(|digests| digests.values().next())
                    .map_or(String::new(), |hex| format!("{hex}  "));
                let target = entry
                    .target
                    .as_ref()
                    .map_or(String::new(), |target| format!(" -> {target}"));
                writeln!(
                    out,
                    "{kind} {:>12}  {digest}{}{}",
                    entry.metadata.size,
                    output::shown(&path),
                    output::shown(&target)
                )
                .map_err(|error| error.to_string())?;
            }
            bohio_client::walk::WalkItem::Failure(failure) => {
                failed = true;
                item_failure(failure.path.as_deref(), &failure.message);
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

/// `bohio remote ... read --follow`: a file of the host printed as it grows, until interrupted or the link ends.
fn follow_command(
    link: &dyn Open,
    read: &RemoteReadArgs,
    client: &Client,
) -> Result<ExitCode, String> {
    let options = bohio_client::follow::FollowOptions {
        tail: read.tail,
        offset: 0,
        by_name: read.follow_name,
        interval: read.interval,
    };
    let following = bohio_client::follow::follow(link, &read.path, &options, Some(client.reply))
        .map_err(|error| explain(&error))?;
    let mut out = std::io::stdout().lock();
    for event in following {
        match event.map_err(|error| explain(&error))? {
            bohio_client::follow::FollowEvent::Data(bytes) => {
                out.write_all(&bytes)
                    .and_then(|()| out.flush())
                    .map_err(|error| error.to_string())?;
            }
            bohio_client::follow::FollowEvent::Truncated(truncated) => {
                if output::json() {
                    output::status(&serde_json::json!({
                        "event": "truncated",
                        "size": truncated.size,
                        "replaced": truncated.replaced,
                    }));
                } else {
                    output::note(&format!(
                        "{} ({} bytes now)",
                        if truncated.replaced {
                            "the file was replaced"
                        } else {
                            "the file was truncated"
                        },
                        truncated.size
                    ));
                }
            }
        }
    }
    // The host or the link ended it: that is not a normal end of a follow.
    Err("the link ended while the file was followed".to_owned())
}

/// `bohio remote ... stats`.
fn stats_command(link: &dyn Open, client: &Client) -> Result<ExitCode, String> {
    let control = Control::open(link).map_err(|error| explain(&error))?;
    let stats = control
        .stats(Some(client.reply))
        .map_err(|error| explain(&error))?;
    if output::json() {
        println!(
            "{}",
            serde_json::to_string(&stats).map_err(|_| "the statistics cannot be written")?
        );
    } else {
        print_stats(&stats);
    }
    Ok(ExitCode::SUCCESS)
}

fn print_stats(stats: &bohio_proto::control::Stats) {
    println!(
        "host    {}",
        output::shown(stats.hostname.as_deref().unwrap_or("-"))
    );
    let (days, rest) = (stats.uptime_s / 86_400, stats.uptime_s % 86_400);
    println!("uptime  {days}d {}h {}m", rest / 3600, (rest % 3600) / 60);
    println!("cpus    {}", stats.cpus);
    match stats.load {
        Some([one, five, fifteen]) => println!("load    {one:.2} {five:.2} {fifteen:.2}"),
        None => println!("load    -"),
    }
    println!(
        "memory  {} available of {}",
        size_text(stats.memory.available),
        size_text(stats.memory.total)
    );
    println!(
        "swap    {} free of {}",
        size_text(stats.swap.free),
        size_text(stats.swap.total)
    );
    for disk in &stats.disks {
        println!(
            "disk    {}  {}  {} free of {}",
            output::shown(&disk.mount),
            output::shown(if disk.filesystem.is_empty() {
                "-"
            } else {
                &disk.filesystem
            }),
            size_text(disk.available),
            size_text(disk.total)
        );
    }
}

/// A byte count as a person reads it: `12.5 GiB`.
fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// `bohio remote ... which`.
fn which_command(
    link: &dyn Open,
    args: &RemoteWhichArgs,
    client: &Client,
) -> Result<ExitCode, String> {
    let control = Control::open(link).map_err(|error| explain(&error))?;
    let names: Vec<&str> = args.names.iter().map(String::as_str).collect();
    let found = control
        .which(&names, Some(client.reply))
        .map_err(|error| explain(&error))?;
    if output::json() {
        output::object(&serde_json::json!({ "found": found }));
    } else {
        for name in &args.names {
            println!(
                "{}  {}",
                output::shown(name),
                output::shown(found.get(name).and_then(Option::as_deref).unwrap_or("-"))
            );
        }
    }
    Ok(if found.values().all(Option::is_some) {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// `bohio remote ... glob`: the paths below the roots that match a glob.
fn glob_command(
    link: &dyn Open,
    args: &RemoteGlobArgs,
    client: &Client,
) -> Result<ExitCode, String> {
    let options = bohio_client::walk::WalkOptions {
        depth: args.depth,
        include: vec![args.glob.clone()],
        ..bohio_client::walk::WalkOptions::default()
    };
    let roots: Vec<&str> = args.paths.iter().map(String::as_str).collect();
    let iter = bohio_client::walk::walk_with(link, &roots, &options, Some(client.reply))
        .map_err(|error| explain(&error))?;
    let mut out = std::io::stdout().lock();
    let mut failed = false;
    for item in iter {
        match item.map_err(|error| explain(&error))? {
            bohio_client::walk::WalkItem::Entry { root, entry } => {
                // The root itself is not a match.
                if entry.path.is_empty() {
                    continue;
                }
                let base = roots
                    .get(usize::try_from(root).unwrap_or(usize::MAX))
                    .copied()
                    .unwrap_or("");
                let path = joined(base, &entry.path);
                if output::json() {
                    output::event(
                        "path",
                        serde_json::json!({
                            "root": root,
                            "path": path,
                            "kind": entry.metadata.kind,
                            "size": entry.metadata.size,
                        }),
                    );
                } else {
                    writeln!(out, "{}", output::shown(&path)).map_err(|error| error.to_string())?;
                }
            }
            bohio_client::walk::WalkItem::Failure(failure) => {
                failed = true;
                item_failure(failure.path.as_deref(), &failure.message);
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

/// `base` and a path below it, joined with `/`.
fn joined(base: &str, path: &str) -> String {
    if path.is_empty() {
        base.to_owned()
    } else {
        format!("{}/{path}", base.trim_end_matches('/'))
    }
}

/// `bohio remote ... search`: the matching lines of files, found on the host. No match is success (the host's counts say
/// so, in the `done` event); a file or directory it could not read is exit 1.
fn search_command(
    link: &dyn Open,
    args: &RemoteSearchArgs,
    client: &Client,
) -> Result<ExitCode, String> {
    let options = bohio_client::search::SearchOptions {
        depth: args.depth,
        include: args.include.clone(),
        exclude: args.exclude.clone(),
        hidden: args.hidden,
        literal: args.fixed_strings,
        case_insensitive: args.ignore_case,
        context: args.context,
        max_matches: args.max_matches,
        max_files: args.max_files,
    };
    let roots: Vec<&str> = args.paths.iter().map(String::as_str).collect();
    let mut iter = bohio_client::search::search_with(
        link,
        &roots,
        &args.pattern,
        &options,
        Some(client.reply),
    )
    .map_err(|error| explain(&error))?;
    let mut out = std::io::stdout().lock();
    let mut failed = false;
    for item in iter.by_ref() {
        match item.map_err(|error| explain(&error))? {
            bohio_client::search::SearchItem::Match(hit) => {
                let base = roots
                    .get(usize::try_from(hit.root).unwrap_or(usize::MAX))
                    .copied()
                    .unwrap_or("");
                let path = joined(base, &hit.path);
                if output::json() {
                    output::event(
                        "match",
                        serde_json::json!({
                            "root": hit.root,
                            "path": path,
                            "line": hit.hit.line,
                            "text": hit.hit.text,
                            "before": hit.hit.before,
                            "after": hit.hit.after,
                            "truncated": hit.hit.truncated,
                        }),
                    );
                } else {
                    write_hit(&mut out, &path, &hit.hit).map_err(|error| error.to_string())?;
                }
            }
            bohio_client::search::SearchItem::Failure(failure) => {
                failed = true;
                item_failure(failure.path.as_deref(), &failure.message);
            }
        }
    }
    out.flush().map_err(|error| error.to_string())?;
    if let Some(done) = iter.done() {
        if output::json() {
            output::event("done", serde_json::json!(done));
        } else if done.truncated {
            output::note(&format!(
                "the search stopped at a bound of the host after {} files and {} matches",
                done.files, done.matches
            ));
        }
    }
    Ok(if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// One match as `path:line:text`, its context lines as `path-line-text`.
fn write_hit(
    out: &mut impl Write,
    path: &str,
    hit: &bohio_proto::search::SearchMatch,
) -> std::io::Result<()> {
    let first = hit.line.saturating_sub(hit.before.len() as u64);
    let path = &*output::shown(path);
    for (offset, line) in hit.before.iter().enumerate() {
        writeln!(out, "{path}-{}-{line}", first + offset as u64)?;
    }
    writeln!(out, "{path}:{}:{}", hit.line, hit.text)?;
    for (offset, line) in hit.after.iter().enumerate() {
        writeln!(out, "{path}-{}-{line}", hit.line + 1 + offset as u64)?;
    }
    Ok(())
}

/// `bohio remote ... sync`: a tree and a tree of the host made the same.
fn sync_command(
    link: &dyn Open,
    args: &RemoteSyncArgs,
    client: &Client,
) -> Result<ExitCode, String> {
    use bohio_client::sync::{Direction, Step, TreeOptions, sync_tree};
    let defaults = client.profile.defaults();
    let compression = match wanted_or(args.compress.as_deref(), None, defaults.compression)? {
        Some(compression) if compression.algorithm == Codec::None => None,
        Some(compression) if args.direction == SyncDirection::Push => {
            Some(named(link, compression, client)?)
        }
        other => other,
    };
    let sync = SyncOptions {
        compression: compression.clone(),
        timeout: client.transfer,
        read_size: client.read_size,
        ..SyncOptions::default()
    };
    let options = TreeOptions {
        direction: match args.direction {
            SyncDirection::Push => Direction::Push,
            SyncDirection::Pull => Direction::Pull,
        },
        compare: args
            .compare
            .map(|how| match how {
                super::CompareChoice::Meta => Compare::Meta,
                super::CompareChoice::Hash => Compare::Hash(Algorithm::Blake3),
                super::CompareChoice::Blocks => Compare::Blocks,
            })
            .or(defaults.compare)
            .unwrap_or(Compare::Meta),
        delete: args.delete,
        dry_run: args.dry_run,
        bundle_max: args.bundle_max.unwrap_or(client.bundle.0),
        bundle_items: args.bundle_items.unwrap_or(client.bundle.1),
        parallel: args.parallel,
        sync,
        bundle: bohio_client::bundle::BundleOptions {
            compression,
            timeout: client.transfer,
            ..bohio_client::bundle::BundleOptions::default()
        },
        ..TreeOptions::default()
    };
    let report =
        sync_tree(link, &args.local, &args.remote, &options).map_err(|error| explain(&error))?;
    if output::json() {
        let step = |step: &Step| match step {
            Step::MakeDirectory => "mkdir",
            Step::Create => "create",
            Step::Update => "update",
            Step::Delete => "delete",
        };
        output::object(&serde_json::json!({
            "unchanged": report.unchanged,
            "bundled": report.bundled,
            "bundles": report.bundles,
            "singles": report.singles,
            "deltas": report.deltas,
            "deleted": report.deleted,
            "skipped": report.skipped,
            "bytes": report.bytes,
            "wire_bytes": report.wire_bytes,
            "dry_run": args.dry_run,
            "actions": report.actions.iter().map(|action| serde_json::json!({
                "step": step(&action.step),
                "path": action.path,
                "size": action.size,
            })).collect::<Vec<_>>(),
            "failures": report.failures.iter().map(|(path, why)| serde_json::json!({
                "path": path,
                "message": why,
            })).collect::<Vec<_>>(),
        }));
        return Ok(if report.is_clean() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        });
    }
    let mut out = std::io::stdout().lock();
    if args.dry_run {
        for action in &report.actions {
            let verb = match action.step {
                Step::MakeDirectory => "mkdir",
                Step::Create => "create",
                Step::Update => "update",
                Step::Delete => "delete",
            };
            writeln!(
                out,
                "{verb} {} ({} bytes)",
                output::shown(&action.path),
                action.size
            )
            .map_err(|error| error.to_string())?;
        }
    }
    writeln!(
        out,
        "{} unchanged, {} bundled in {} bundles, {} large ({} by delta), {} deleted, {} skipped; {} bytes of files, {} on the wire",
        report.unchanged,
        report.bundled,
        report.bundles,
        report.singles,
        report.deltas,
        report.deleted,
        report.skipped,
        report.bytes,
        report.wire_bytes
    )
    .map_err(|error| error.to_string())?;
    out.flush().map_err(|error| error.to_string())?;
    for (path, why) in &report.failures {
        eprintln!(
            "bohio: {}: {}",
            output::shown(path),
            output::shown_lines(why)
        );
    }
    Ok(if report.is_clean() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
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
        output::note(&format!(
            "the host did not deliver {}: {}",
            answer.signal,
            answer.message.as_deref().unwrap_or("it cannot here")
        ));
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
        return terminal_command(
            link,
            request,
            args.kill_after.unwrap_or(client.kill_after),
            client,
        );
    }
    let mut request = Request::argv(args.argv.iter().cloned());
    request.session = wanted_session(&args.session);
    request.source.clone_from(&args.source);
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
    drive_exec(
        exec,
        args.stdin,
        args.kill_after.unwrap_or(client.kill_after),
        client,
    )
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
    if output::json() {
        output::object(&serde_json::json!({ "session": id }));
    } else {
        println!("{id}");
    }
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
            Ok(Some(Event::Started { pid })) if output::json() => {
                output::event("started", serde_json::json!({ "pid": pid }));
            }
            Ok(Some(Event::Stdout(bytes))) if output::json() => {
                output::event(
                    "stdout",
                    serde_json::json!({ "b64": output::base64(&bytes) }),
                );
            }
            Ok(Some(Event::Stderr(bytes))) if output::json() => {
                output::event(
                    "stderr",
                    serde_json::json!({ "b64": output::base64(&bytes) }),
                );
            }
            Ok(Some(Event::Exit(exit))) if output::json() => {
                output::event("exit", serde_json::json!(exit));
                return Ok(status_of(&exit));
            }
            Ok(Some(Event::Signalled(answer))) if output::json() => {
                output::event("signalled", serde_json::json!(answer));
            }
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
    terminal_command(
        link,
        request,
        args.kill_after.unwrap_or(client.kill_after),
        client,
    )
}

/// `bohio remote ... power`.
fn power_command(
    link: &dyn Open,
    args: &super::RemotePowerArgs,
    client: &Client,
) -> Result<ExitCode, String> {
    use super::PowerWord;
    use bohio_proto::power::Action;
    let action = match args.action {
        PowerWord::Status => Action::Status,
        PowerWord::On => Action::On,
        PowerWord::Off => Action::Off,
        PowerWord::SoftOff => Action::SoftOff,
        PowerWord::Cycle => Action::Cycle,
        PowerWord::Reset => Action::Reset,
        PowerWord::Nmi => Action::Nmi,
    };
    // An action may take as long as the host's `[power] action_timeout` and then says so; this waits a little longer.
    let wait = client.reply.max(client.power_wait);
    let state = bohio_client::power::act(link, args.source.as_deref(), action, Some(wait))
        .map_err(|error| explain(&error))?;
    println!("{}", power_text(&state, output::json())?);
    Ok(ExitCode::SUCCESS)
}

/// What `remote power` prints: `{state, done}` for a program, else the state and whether it is final.
fn power_text(state: &bohio_proto::power::State, json: bool) -> Result<String, String> {
    if json {
        return serde_json::to_string(state).map_err(|_| "the answer cannot be written".to_owned());
    }
    Ok(format!(
        "{}{}",
        state.state,
        if state.done { "" } else { " (not finished)" }
    ))
}

/// What `remote media` prints: `{slots: [...]}` for a program, else one line for each slot.
fn media_text(slots: &[bohio_proto::media::Slot], json: bool) -> Result<String, String> {
    if json {
        return serde_json::to_string(&serde_json::json!({ "slots": slots }))
            .map_err(|_| "the answer cannot be written".to_owned());
    }
    Ok(slots
        .iter()
        .map(|slot| {
            format!(
                "{} {} {}",
                output::shown(&slot.slot),
                if slot.inserted { "inserted" } else { "empty" },
                output::shown(slot.image.as_deref().unwrap_or("-"))
            )
        })
        .collect::<Vec<_>>()
        .join(
            "
",
        ))
}

/// `bohio remote ... media`.
fn media_command(
    link: &dyn Open,
    args: &super::RemoteMediaArgs,
    client: &Client,
) -> Result<ExitCode, String> {
    use super::MediaOp;
    use bohio_proto::media::Image;
    let wait = Some(client.reply.max(client.power_wait));
    let source = args.source.as_deref();
    let slots = match &args.op {
        MediaOp::Status => bohio_client::media::status(link, source, wait),
        MediaOp::Insert(insert) => {
            let image = match (&insert.url, &insert.path) {
                (_, Some(path)) => Image::Path { path: path.clone() },
                (Some(url), None) => Image::Url { url: url.clone() },
                (None, None) => return Err("name a URL, or a file with --path".to_owned()),
            };
            bohio_client::media::insert(link, source, &insert.slot, image, wait)
        }
        MediaOp::Eject(eject) => bohio_client::media::eject(link, source, &eject.slot, wait),
    }
    .map_err(|error| explain(&error))?;
    let text = media_text(&slots, output::json())?;
    if !text.is_empty() {
        println!("{text}");
    }
    Ok(ExitCode::SUCCESS)
}

/// `bohio remote ... console`: the node's console of a terminal source, left with Ctrl-] then q.
fn console_command(
    link: &dyn Open,
    args: &RemoteConsoleArgs,
    client: &Client,
) -> Result<ExitCode, String> {
    let interactive = std::io::stdin().is_terminal();
    let size = if interactive { local_size() } else { (80, 24) };
    let shell = if let Some(from) = args.from {
        let id = Shell::console_session(link, args.source.as_deref(), Some(client.reply))
            .map_err(|error| explain(&error))?;
        let (shell, attached) =
            Shell::attach(link, &id, Some(from)).map_err(|error| explain(&error))?;
        if attached.lost > 0 {
            eprintln!(
                "bohio: the host had forgotten {} bytes before offset {}: the output starts there",
                attached.lost, attached.from
            );
        }
        shell
    } else {
        let mut request = ShellRequest::console(args.source.as_deref());
        request.cols = size.0;
        request.rows = size.1;
        Shell::start(link, &request, Some(client.reply)).map_err(|error| explain(&error))?
    };
    if interactive {
        eprintln!("bohio: connected to the console; Ctrl-] then q leaves it");
    }
    drive_terminal(
        &Arc::new(shell),
        interactive,
        size,
        args.kill_after.unwrap_or(client.kill_after),
        client,
        true,
    )
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
        if output::json() {
            output::object(&serde_json::json!({ "session": id }));
        } else {
            println!("{id}");
        }
        return Ok(ExitCode::SUCCESS);
    }
    drive_terminal(&shell, interactive, size, kill_after, client, false)
}

/// Bridges a terminal session of the host to this one until the program ends, and returns its status.
fn drive_terminal(
    shell: &Arc<Shell>,
    interactive: bool,
    size: (u16, u16),
    kill_after: Duration,
    client: &Client,
    escape: bool,
) -> Result<ExitCode, String> {
    // A console is left with Ctrl-] then q (telnet's pair); what is typed after Ctrl-] and is not q goes to the host.
    let leave = Arc::new(std::sync::atomic::AtomicBool::new(false));
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
        let leave = Arc::clone(&leave);
        let chunk = client.input_chunk.max(1);
        thread::spawn(move || {
            let mut buffer = vec![0_u8; chunk];
            let mut input = std::io::stdin().lock();
            let mut escaped = false;
            while let Ok(count) = input.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                let mut typed = Vec::with_capacity(count + 1);
                for &byte in &buffer[..count] {
                    if escape && escaped {
                        escaped = false;
                        if byte == b'q' {
                            leave.store(true, std::sync::atomic::Ordering::Release);
                            return;
                        }
                        typed.push(0x1d);
                    }
                    if escape && byte == 0x1d {
                        escaped = true;
                    } else {
                        typed.push(byte);
                    }
                }
                if !typed.is_empty() && shell.write(&typed).is_err() {
                    break;
                }
            }
        });
    }
    let result = loop {
        if leave.load(std::sync::atomic::Ordering::Acquire) {
            if raw {
                let _ = crossterm::terminal::disable_raw_mode();
            }
            shell.close();
            return Ok(ExitCode::SUCCESS);
        }
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
    if output::json() {
        output::status(&serde_json::json!({ "done": { "exit": exit } }));
    }
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
        output::note(&format!(
            "the host had forgotten {} bytes before offset {}: the output starts there",
            attached.lost, attached.from
        ));
    }
    match attachment {
        bohio_client::session::Attachment::Exec(exec) => drive_exec(
            Arc::from(exec),
            args.stdin,
            args.kill_after.unwrap_or(client.kill_after),
            client,
        ),
        bohio_client::session::Attachment::Shell(shell) => {
            let interactive = std::io::stdin().is_terminal();
            let size = if interactive { local_size() } else { (80, 24) };
            if interactive {
                // The terminal of a shell that was started elsewhere takes this one's size.
                let _ = shell.resize(size.0, size.1);
            }
            drive_terminal(
                &Arc::new(shell),
                interactive,
                size,
                args.kill_after.unwrap_or(client.kill_after),
                client,
                false,
            )
        }
        _ => Err("this version does not know that kind of session".to_owned()),
    }
}

/// `bohio remote ... sessions`.
fn sessions_command(link: &dyn Open, client: &Client) -> Result<ExitCode, String> {
    let control = Control::open(link).map_err(|error| explain(&error))?;
    let sessions = control
        .sessions(Some(client.reply))
        .map_err(|error| explain(&error))?;
    if output::json() {
        output::object(&serde_json::json!({ "sessions": sessions }));
    } else {
        {
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
                    output::shown(session.name.as_deref().unwrap_or("-")),
                    output::shown(&session.program),
                    session
                        .peer
                        .as_deref()
                        .map_or(String::new(), |peer| format!("\t{}", output::shown(peer))),
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
    if output::json() {
        output::object(&serde_json::json!({ "ended": args.id }));
    }
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
    let wait = match args.wait {
        Some(wait) => wait,
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
    if output::json() {
        println!(
            "{}",
            serde_json::to_string(&reply).map_err(|_| "the links cannot be written")?
        );
    } else {
        {
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
                let theirs = output::shown(&theirs);
                println!(
                    "{} {} {at} offers-to-peer: {} offers-from-peer: {theirs}",
                    link.peer,
                    link.side,
                    output::shown(&list(&link.mine))
                );
            }
        }
    }
    Ok(())
}

/// `bohio providers`.
pub(super) fn providers(
    args: &super::ProvidersArgs,
    environment: &Environment,
) -> Result<(), String> {
    let (endpoint, settings, server) = control_endpoint(&args.config, environment)?;
    let local = crate::local::LocalLink::connect_to(&endpoint, &settings, server)
        .map_err(|error| unreachable(&error))?;
    let reply = local.providers().map_err(|error| explain(&error))?;
    if output::json() {
        println!(
            "{}",
            serde_json::to_string(&reply).map_err(|_| "the providers cannot be written")?
        );
        return Ok(());
    }
    if !reply.check_access {
        println!("non-default: [providers] check_access is false");
    }
    if reply.libraries.is_empty() && reply.programs.is_empty() {
        println!(
            "no provider libraries or programs (`[providers] paths` and `[[providers.program]]` are empty)"
        );
    }
    for library in &reply.libraries {
        println!(
            "{} {} {} ({})",
            library.state,
            library.path,
            library.detail,
            library.digest.as_deref().unwrap_or("no digest")
        );
    }
    for program in &reply.programs {
        println!(
            "{} program {} {} {} ({}, {} restart{})",
            program.state,
            program.name,
            program.path,
            program.detail,
            program.digest.as_deref().unwrap_or("no digest"),
            program.restarts,
            if program.restarts == 1 { "" } else { "s" }
        );
    }
    Ok(())
}

/// How `args` reaches its host, and the tool settings its configuration gives: the waits of `[client]` and of the forwards.
pub(super) fn dialer(
    args: &RemoteArgs,
    environment: &Environment,
) -> Result<(Dialer, bohio_client::tools::Settings), String> {
    let plan = match &args.via {
        Some(id) => via_plan(id, args, environment)?,
        None => direct_plan(args, environment)?,
    };
    let found = source(&args.config, environment)?.0;
    let client = Client::new(found.config(), args);
    let mut settings = bohio_client::tools::Settings::default();
    settings.reply_timeout = client.reply;
    settings.transfer_timeout = client.transfer;
    settings.gui_timeout = client.gui;
    settings.poll = client.poll;
    settings.forward = found.config().forward.client();
    Ok((Box::new(move || plan.dial()), settings))
}

/// Tells where a forward listens: `KIND ADDRESS -> TARGET` (`socks5 ADDRESS` has no target), or an event under `--format json`.
fn announce(kind: &str, address: &str, to: Option<&str>) {
    if output::json() {
        output::event(
            "listening",
            serde_json::json!({ "kind": kind, "address": address, "target": to }),
        );
        return;
    }
    match to {
        Some(to) => println!("{kind} {address} -> {to}"),
        None => println!("{kind} {address}"),
    }
    let _ = std::io::stdout().flush();
}

/// Tells that a listener is bound on an address the operator named, not the loopback default.
fn non_default(bind: &str) {
    if output::json() {
        output::event("non_default", serde_json::json!({ "bind": bind }));
    } else {
        println!("non-default: bind {bind}");
    }
}

/// The end of a command whose output is its payload: under `--format json` the final status, one object on standard error.
fn done_status() -> ExitCode {
    if output::json() {
        output::status(&serde_json::json!({ "done": {} }));
    }
    ExitCode::SUCCESS
}

/// Something a streaming command could not read, and went on: `bohio: PATH: MESSAGE` on standard error, or under `--format json` a
/// `failure` event in the stream.
fn item_failure(path: Option<&str>, message: &str) {
    if output::json() {
        output::event(
            "failure",
            serde_json::json!({ "path": path, "message": message }),
        );
    } else {
        eprintln!(
            "bohio: {}: {}",
            output::shown(path.unwrap_or("")),
            output::shown_lines(message)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{media_text, power_text};
    use bohio_proto::media::Slot;
    use bohio_proto::power::State;

    #[test]
    fn power_and_media_answer_as_objects_under_json_and_as_lines_otherwise() {
        let state = State {
            state: "on".to_owned(),
            done: false,
        };
        assert_eq!(power_text(&state, false).unwrap(), "on (not finished)");
        let json: serde_json::Value =
            serde_json::from_str(&power_text(&state, true).unwrap()).unwrap();
        assert_eq!(json["state"], "on");
        assert_eq!(json["done"], false);

        let slots = vec![Slot {
            slot: "cd0".to_owned(),
            inserted: true,
            image: Some("boot.iso".to_owned()),
            size: None,
        }];
        assert_eq!(media_text(&slots, false).unwrap(), "cd0 inserted boot.iso");
        let json: serde_json::Value =
            serde_json::from_str(&media_text(&slots, true).unwrap()).unwrap();
        assert_eq!(json["slots"][0]["slot"], "cd0");
        assert_eq!(json["slots"][0]["inserted"], true);
    }
}
