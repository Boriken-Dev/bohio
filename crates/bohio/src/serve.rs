//! Serving links: accepting connections, proving each peer and dispatching its streams to the engines.
//!
//! A connection proves itself first, within `[link] proof_timeout`, holding an unproven peer to one
//! small message. A key that proves itself and is not accepted is recorded as waiting by the proof
//! (see `bohio peer list`) and the connection ends. An accepted peer's link is then served: each
//! stream it opens is decided by [`crate::link`] at open and at every `[link] recheck`, and runs one
//! engine behind a bounded [`Relay`](bohio_client::relay::Relay).

use std::ffi::OsString;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use bohio_client::link::{Settings, socket};
use bohio_client::relay::relay;
use bohio_proto::info::Info;
use bohio_proto::protocol;
use coqui_link::auth::{Proof, Sealed, Trust, prove};
use coqui_link::link::{Accepted as Open, Host, Options, Services, serve as serve_link};
use coqui_link::mux::{CloseReason, LinkStream, Side};
use coqui_peer::{Handshake, Method as ProofMethod, PeerId};
use coqui_transport::Channel;

use crate::config::{Config, Environment};
use crate::limit::Slots;
use crate::link::{LogEvents, PROTOCOLS, TrustPeers, admission, grant_of};
use crate::peer::{Data, PeerError};
use crate::policy::Grant;
use crate::{control, exec, fs, info, shell};

/// A place to listen or connect to: `tcp://host:port`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Endpoint {
    /// A TCP socket.
    Tcp(SocketAddr),
}

impl Endpoint {
    /// Reads `scheme://address`; a name is resolved and its first address taken.
    ///
    /// # Errors
    ///
    /// The reason, for a scheme this build does not serve or an address that does not resolve.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (scheme, address) = text
            .split_once("://")
            .ok_or_else(|| "an endpoint looks like `tcp://host:port`".to_owned())?;
        match scheme {
            "tcp" => address
                .to_socket_addrs()
                .map_err(|error| format!("`{address}` cannot be resolved: {error}"))?
                .next()
                .map(Self::Tcp)
                .ok_or_else(|| format!("`{address}` has no address")),
            other => Err(format!("the scheme `{other}` is not served; use `tcp`")),
        }
    }
}

impl core::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Tcp(address) => write!(f, "tcp://{address}"),
        }
    }
}

/// Why a host could not start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ServeError {
    /// The identity, the trust store or `peers.cfg` could not be used.
    #[error("{0}")]
    Peer(PeerError),
    /// The proof could not be set up.
    #[error("the proof cannot be set up: {0}")]
    Proof(String),
    /// A listener could not be opened.
    #[error("cannot listen on {endpoint}: {reason}")]
    Listen {
        /// The endpoint.
        endpoint: String,
        /// The operating system's reason.
        reason: String,
    },
}

impl From<PeerError> for ServeError {
    fn from(error: PeerError) -> Self {
        Self::Peer(error)
    }
}

/// What every connection shares.
struct Shared {
    config: Config,
    settings: Settings,
    policy: std::path::PathBuf,
    path: OsString,
    info: Info,
    processes: Slots,
    transfers: Slots,
    sessions: Slots,
}

/// A running host: its identity, its trust store and the engines it serves.
pub struct Server {
    shared: Arc<Shared>,
    proof: Proof,
    peers: TrustPeers,
    store: coqui_peer::TrustStore,
    services: Services,
    host: PeerId,
    connections: AtomicUsize,
}

impl core::fmt::Debug for Server {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Server")
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

/// A proof by `Proof` that tells the handshake where the peer connected from, so a key that proves
/// itself and is not accepted waits under its own source rather than a shared one.
struct Sourced<'a> {
    proof: &'a Proof,
    source: Option<SocketAddr>,
}

impl ProofMethod for Sourced<'_> {
    type Handshake = <Proof as ProofMethod>::Handshake;

    fn name(&self) -> &'static str {
        self.proof.name()
    }

    fn initiate(&self) -> Result<Self::Handshake, <Self::Handshake as Handshake>::Error> {
        self.proof.initiate()
    }

    fn respond(
        &self,
        _source: Option<SocketAddr>,
    ) -> Result<Self::Handshake, <Self::Handshake as Handshake>::Error> {
        self.proof.respond(self.source)
    }
}

impl Server {
    /// Opens (or makes) the host's identity and trust store under the data directory `config` names and
    /// prepares one service per protocol.
    ///
    /// # Errors
    ///
    /// [`ServeError::Peer`] when the identity or the trust store cannot be used, [`ServeError::Proof`]
    /// when the proof cannot be built.
    pub fn new(config: &Config, environment: &Environment) -> Result<Self, ServeError> {
        let data = Data::new(config.data_dir(environment));
        let identity = data.identity()?;
        let store = data.store()?;
        let proof = Proof::builder(identity.clone())
            .initiators(Trust::Store(store.clone()))
            .build()
            .map_err(|error| ServeError::Proof(error.to_string()))?;
        let host = identity.peer_id();
        let mut capabilities = info::collect(config, environment);
        capabilities.host_id = Some(host.to_string());
        let shared = Arc::new(Shared {
            settings: config.link.settings(),
            policy: data.policy_path(),
            path: environment.path.clone().unwrap_or_default(),
            info: capabilities,
            processes: Slots::new(config.execution.max_processes),
            transfers: Slots::new(config.filesystem.max_transfers),
            sessions: Slots::new(config.shell.max_sessions),
            config: config.clone(),
        });
        let mut services = Services::default();
        for (token, needs) in PROTOCOLS {
            let shared = Arc::clone(&shared);
            services = services.with_admission(
                token,
                Box::new(move |stream: &dyn LinkStream, accepted: &Open| {
                    stream_served(&shared, stream, accepted);
                }),
                admission(data.policy_path(), needs),
            );
        }
        Ok(Self {
            shared,
            proof,
            peers: TrustPeers::new(store.clone()),
            store,
            services,
            host,
            connections: AtomicUsize::new(0),
        })
    }

    /// This host's id.
    pub const fn host(&self) -> PeerId {
        self.host
    }

    /// Accepts connections on `listener` until `stop` is set, serving each on a thread of its own; at
    /// most `[link] max_connections` at once, a connection past that is closed unread.
    ///
    /// # Errors
    ///
    /// The listener's error.
    pub fn serve_listener(&self, listener: &TcpListener, stop: &AtomicBool) -> std::io::Result<()> {
        listener.set_nonblocking(true)?;
        let limit = self.shared.config.link.max_connections;
        thread::scope(|scope| {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((stream, source)) => {
                        if self.connections.fetch_add(1, Ordering::SeqCst) >= limit {
                            self.connections.fetch_sub(1, Ordering::SeqCst);
                            tracing::info!("a connection was closed: max_connections are open");
                            continue;
                        }
                        scope.spawn(move || {
                            self.connection(stream, source);
                            self.connections.fetch_sub(1, Ordering::SeqCst);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        })
    }

    fn connection(&self, stream: TcpStream, source: SocketAddr) {
        let stream = stream.set_nonblocking(false).map(|()| stream);
        let channel = match stream.and_then(|stream| socket(stream, &self.shared.settings)) {
            Ok(channel) => channel,
            Err(error) => return tracing::info!("a connection could not be set up: {error}"),
        };
        self.serve_channel(&channel, Some(source));
        channel.close();
    }

    /// Proves the peer on `channel` and serves its link until it ends. The caller closes `channel`.
    pub fn serve_channel<C: Channel>(&self, channel: &C, source: Option<SocketAddr>) {
        let settings = &self.shared.settings;
        let sourced = Sourced {
            proof: &self.proof,
            source,
        };
        let established = match prove(
            &sourced,
            channel,
            settings.proof_timeout,
            settings.record_length(self.proof.method()),
        ) {
            Ok(established) => established,
            Err(error @ coqui_link::Error::Proof(_)) => {
                return tracing::info!(
                    "a key was refused: {error}; a key that proved itself and is not accepted waits for `bohio peer approve`"
                );
            }
            Err(error) => return tracing::info!("a connection ended before its proof: {error}"),
        };
        let _ = channel.set_pending(settings.read_ahead);
        let peer = established.peer;
        let recheck = self.shared.config.link.recheck.0;
        let store = self.store.clone();
        // A revoked key's streams end at the next re-check; the link itself ends at the second.
        let sealed = Sealed::new(channel, established.session)
            .with_check(move || store.is_accepted(&peer))
            .with_check_every(recheck.saturating_mul(2));
        let mut options = Options::default();
        options.limits = settings.limits();
        options.recheck = recheck;
        let host = Host {
            store: &self.peers,
            services: &self.services,
            events: &LogEvents,
        };
        serve_link(
            &sealed,
            Side::Answered,
            options,
            None,
            &peer.to_string(),
            &host,
        );
        sealed.close();
    }
}

/// Runs one stream's engine behind a relay, under the grant `peers.cfg` gives the peer now.
fn stream_served(shared: &Shared, stream: &dyn LinkStream, accepted: &Open) {
    let grant: Grant = match grant_of(&shared.policy, &accepted.peer) {
        Ok(grant) => grant,
        Err(error) => {
            tracing::warn!("the peer grants cannot be read: {error}");
            stream.close_with(CloseReason::Refused, "the host cannot read its peer grants");
            return;
        }
    };
    thread::scope(|scope| {
        let relay = relay(scope, stream, &shared.settings);
        let channel: Arc<dyn Channel> = relay.clone();
        let metadata = accepted.metadata.as_slice();
        let config = &shared.config;
        let files = fs::Context {
            grant: &grant,
            config: &config.filesystem,
            slots: &shared.transfers,
        };
        match accepted.protocol.as_str() {
            protocol::EXEC => {
                let context = exec::Context {
                    grant: &grant,
                    config: &config.execution,
                    slots: &shared.processes,
                    path: &shared.path,
                };
                match exec::serve(channel, metadata, &context) {
                    Ok(exit) => tracing::debug!(code = ?exit.code, "exec ended"),
                    Err(error) => tracing::info!(code = ?error.code, "exec refused"),
                }
            }
            protocol::SHELL => {
                let context = shell::Context {
                    grant: &grant,
                    config: &config.shell,
                    slots: &shared.sessions,
                    path: &shared.path,
                };
                match shell::serve(channel, metadata, &context) {
                    Ok(exit) => tracing::debug!(code = ?exit.code, "shell ended"),
                    Err(error) => tracing::info!(code = ?error.code, "shell refused"),
                }
            }
            protocol::READ => {
                if let Err(error) = fs::serve_read(channel, metadata, &files) {
                    tracing::info!(code = ?error.code, "read refused");
                }
            }
            protocol::WRITE => {
                if let Err(error) = fs::serve_write(channel, metadata, &files) {
                    tracing::info!(code = ?error.code, "write refused");
                }
            }
            protocol::CONTROL => {
                let context = control::Context {
                    filesystem: files,
                    info: &shared.info,
                };
                let answered = control::serve(&channel, &context);
                tracing::debug!(answered, "control ended");
            }
            _ => {}
        }
        relay.close();
    });
}
