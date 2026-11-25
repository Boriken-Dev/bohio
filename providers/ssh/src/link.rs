// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The connection to one host: the runtime that drives it, the host-key check, the login and the channels.

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use bohio_api::guatiao::Status;
use bohio_api::guatiao::library::ProviderError;
use bohio_api::unavailable;
use russh::client::{self, Handle, Msg};
use russh::keys::{
    Algorithm, HashAlg, PrivateKeyWithHashAlg, PublicKey, PublicKeyOrCertificate, load_secret_key,
};
use russh::{Channel, ChannelOpenFailure};
use tokio::runtime::Runtime;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::forward::Forward;
use crate::settings::{HostKeyCheck, Settings, home};

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(crate) fn refuse(status: Status, message: impl core::fmt::Display) -> ProviderError {
    ProviderError::new(status, &message.to_string())
}

/// The runtime a source's connection runs on. Dropping the last holder stops it without waiting for tasks.
pub(crate) struct Rt(Option<Runtime>);

impl Rt {
    pub(crate) fn new(threads: usize) -> Result<Arc<Self>, ProviderError> {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(threads)
            .thread_name("bohio-ssh")
            .enable_all()
            .build()
            .map(|runtime| Arc::new(Self(Some(runtime))))
            .map_err(|_| refuse(Status::GUATIAO_ERR_INTERNAL, "the runtime could not start"))
    }

    pub(crate) fn handle(&self) -> tokio::runtime::Handle {
        self.0
            .as_ref()
            .map(Runtime::handle)
            .cloned()
            .unwrap_or_else(|| unreachable!("a runtime is taken only when it is dropped"))
    }

    /// Runs `future` to its end on the calling thread, which must not be one of the runtime's.
    pub(crate) fn block_on<F: Future>(&self, future: F) -> F::Output {
        self.handle().block_on(future)
    }
}

impl Drop for Rt {
    fn drop(&mut self) {
        if let Some(runtime) = self.0.take() {
            runtime.shutdown_background();
        }
    }
}

/// What the handler and the link share across reconnects: the key trusted at first contact, and why a handshake was
/// refused.
#[derive(Default)]
pub(crate) struct HostKeys {
    trusted: Mutex<Option<String>>,
    refusal: Mutex<Option<String>>,
}

pub(crate) struct Client {
    settings: Arc<Settings>,
    keys: Arc<HostKeys>,
    forward: Option<Arc<Forward>>,
}

/// The SHA-256 of the public key blob, as `sha256:HEX`.
pub(crate) fn fingerprint(key: &PublicKey) -> String {
    let digest = key.fingerprint(HashAlg::Sha256);
    let mut text = String::from("sha256:");
    for byte in digest.as_bytes() {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

impl Client {
    fn verdict(&self, key: &PublicKey) -> Result<(), String> {
        let settings = &self.settings;
        let presented = fingerprint(key);
        if settings.check == HostKeyCheck::Off {
            return Ok(());
        }
        if let Some(pin) = &settings.pin {
            return if *pin == presented {
                Ok(())
            } else {
                Err(format!(
                    "the host presented key {presented}, which is not the pinned {pin}"
                ))
            };
        }
        match russh::keys::known_hosts::check_known_hosts_path(
            &settings.host,
            settings.port,
            key,
            &settings.known_hosts,
        ) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(russh::keys::Error::KeyChanged { line }) => {
                return Err(format!(
                    "the host presented key {presented}, which is not the key line {line} of {} holds for it",
                    settings.known_hosts.display()
                ));
            }
            // A missing or unreadable file knows no host.
            Err(_) => {}
        }
        if settings.check == HostKeyCheck::Tofu {
            let mut trusted = lock(&self.keys.trusted);
            return match trusted.as_deref() {
                None => {
                    *trusted = Some(presented);
                    Ok(())
                }
                Some(known) if known == presented => Ok(()),
                Some(known) => Err(format!(
                    "the host presented key {presented}, which is not the {known} trusted at first contact"
                )),
            };
        }
        Err(format!(
            "the host presented key {presented}, which neither `host_key` nor {} holds for it; pin it with `host_key = \"{presented}\"` (after checking it with the host's owner), add it to known_hosts, or set host_key_check = \"tofu\" to trust it at first contact",
            settings.known_hosts.display()
        ))
    }
}

impl client::Handler for Client {
    type Error = russh::Error;

    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        match self.verdict(&server_public_key.public_key()) {
            Ok(()) => Ok(true),
            Err(reason) => {
                *lock(&self.keys.refusal) = Some(reason);
                Ok(false)
            }
        }
    }

    async fn server_channel_open_x11(
        &mut self,
        channel: Channel<Msg>,
        originator_address: &str,
        originator_port: u32,
        reply: client::ChannelOpenHandle,
        _session: &mut client::Session,
    ) -> Result<(), Self::Error> {
        // Without forwarding, or past its limit, the reply is dropped and the host is told no.
        let Some(forward) = self.forward.clone() else {
            return Ok(());
        };
        let Ok(permit) = Arc::clone(&forward.slots).try_acquire_owned() else {
            tracing::warn!(
                "ssh source: an X connection from {originator_address}:{originator_port} was refused: x11_max_connections are open"
            );
            return Ok(());
        };
        reply.accept().await;
        tokio::spawn(async move {
            if let Err(why) = forward.relay(channel).await {
                tracing::warn!("ssh source: a forwarded X connection ended: {why}");
            }
            drop(permit);
        });
        Ok(())
    }
}

/// One authenticated connection.
pub(crate) struct Conn {
    pub(crate) handle: Handle<Client>,
}

impl Conn {
    pub(crate) fn is_closed(&self) -> bool {
        self.handle.is_closed()
    }
}

struct State {
    conn: Option<Arc<Conn>>,
    /// When and why the latest connect failed.
    failed: Option<(Instant, String)>,
    /// A connection was made and then lost, and `reconnect` is off.
    lost: bool,
}

/// Everything one source needs to reach its host.
pub(crate) struct Link {
    pub(crate) settings: Arc<Settings>,
    pub(crate) rt: Arc<Rt>,
    keys: Arc<HostKeys>,
    state: tokio::sync::Mutex<State>,
    permits: Arc<Semaphore>,
    /// What X forwarding needs; `None` when the source does not forward.
    pub(crate) forward: Option<Arc<Forward>>,
}

/// A session channel, and the slot it holds in the source's `max_channels` until it is let go.
pub(crate) struct Slot {
    pub(crate) channel: Channel<Msg>,
    pub(crate) permit: OwnedSemaphorePermit,
}

impl Link {
    pub(crate) fn new(settings: Settings, rt: Arc<Rt>) -> Arc<Self> {
        let permits = Arc::new(Semaphore::new(settings.max_channels));
        let forward = Forward::new(&settings);
        Arc::new(Self {
            settings: Arc::new(settings),
            rt,
            keys: Arc::new(HostKeys::default()),
            state: tokio::sync::Mutex::new(State {
                conn: None,
                failed: None,
                lost: false,
            }),
            permits,
            forward,
        })
    }

    /// `host:port`, for messages.
    pub(crate) fn target(&self) -> String {
        format!("{}:{}", self.settings.host, self.settings.port)
    }

    /// The live connection, made when there is none.
    pub(crate) async fn conn(&self) -> Result<Arc<Conn>, ProviderError> {
        let mut state = self.state.lock().await;
        if let Some(conn) = &state.conn {
            if !conn.is_closed() {
                return Ok(Arc::clone(conn));
            }
            state.conn = None;
            if !self.settings.reconnect {
                state.lost = true;
            }
        }
        if state.lost {
            return Err(unavailable(&format!(
                "{}: the connection was lost and `reconnect` is off",
                self.target()
            )));
        }
        if let Some((when, why)) = &state.failed
            && when.elapsed() < self.settings.reconnect_after
        {
            return Err(unavailable(why));
        }
        match self.connect().await {
            Ok(conn) => {
                let conn = Arc::new(conn);
                state.conn = Some(Arc::clone(&conn));
                state.failed = None;
                Ok(conn)
            }
            Err(why) => {
                state.failed = Some((Instant::now(), why.clone()));
                Err(unavailable(&why))
            }
        }
    }

    /// Forgets `conn` if it is still the current one, so the next use connects again.
    async fn reset(&self, conn: &Arc<Conn>) {
        let mut state = self.state.lock().await;
        if state
            .conn
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, conn))
        {
            state.conn = None;
        }
    }

    /// A session channel: waits for a slot of `max_channels` (at most `operation_timeout`), then opens the channel,
    /// connecting again once when the connection turns out to be dead.
    pub(crate) async fn channel(&self) -> Result<Slot, ProviderError> {
        let wait = self.settings.operation_timeout;
        let permit = tokio::time::timeout(wait, Arc::clone(&self.permits).acquire_owned())
            .await
            .map_err(|_| {
                refuse(
                    Status::GUATIAO_ERR_BUSY,
                    format!(
                        "{}: {} sessions are open, which is max_channels",
                        self.target(),
                        self.settings.max_channels
                    ),
                )
            })?
            .map_err(|_| unavailable("the source has been closed"))?;
        let conn = self.conn().await?;
        let opened = match tokio::time::timeout(wait, conn.handle.channel_open_session()).await {
            Ok(Ok(channel)) => Ok(channel),
            Ok(Err(error)) if !conn.is_closed() => Err(self.open_error(&error)),
            // The connection is gone: make it again and try once more.
            _ => {
                self.reset(&conn).await;
                let conn = self.conn().await?;
                match tokio::time::timeout(wait, conn.handle.channel_open_session()).await {
                    Ok(Ok(channel)) => Ok(channel),
                    Ok(Err(error)) => Err(self.open_error(&error)),
                    Err(_) => Err(refuse(
                        Status::GUATIAO_ERR_GONE,
                        format!(
                            "{}: the server did not open a channel in time",
                            self.target()
                        ),
                    )),
                }
            }
        };
        Ok(Slot {
            channel: opened?,
            permit,
        })
    }

    fn open_error(&self, error: &russh::Error) -> ProviderError {
        if matches!(
            error,
            russh::Error::ChannelOpenFailure(
                ChannelOpenFailure::ConnectFailed
                    | ChannelOpenFailure::ResourceShortage
                    | ChannelOpenFailure::AdministrativelyProhibited
            )
        ) {
            return refuse(
                Status::GUATIAO_ERR_BUSY,
                format!(
                    "{}: the server refused another session channel; its MaxSessions is below max_channels ({})",
                    self.target(),
                    self.settings.max_channels
                ),
            );
        }
        refuse(
            Status::GUATIAO_ERR_GONE,
            format!("{}: {error}", self.target()),
        )
    }

    async fn connect(&self) -> Result<Conn, String> {
        let settings = &self.settings;
        let target = self.target();
        let config = Arc::new(client::Config {
            keepalive_interval: settings.keepalive,
            keepalive_max: settings.keepalive_max,
            nodelay: true,
            ..client::Config::default()
        });
        let handler = Client {
            settings: Arc::clone(&self.settings),
            keys: Arc::clone(&self.keys),
            forward: self.forward.clone(),
        };
        *lock(&self.keys.refusal) = None;
        let timeout = settings.connect_timeout;
        let attempt = tokio::time::timeout(timeout, async {
            let mut handle =
                client::connect(config, (settings.host.as_str(), settings.port), handler)
                    .await
                    .map_err(|error| {
                        lock(&self.keys.refusal)
                            .clone()
                            .unwrap_or_else(|| error.to_string())
                    })?;
            login(&mut handle, settings).await?;
            Ok::<_, String>(handle)
        })
        .await;
        match attempt {
            Err(_) => Err(format!(
                "{target}: no answer within {} ms",
                timeout.as_millis()
            )),
            Ok(Err(why)) => Err(format!("{target}: {why}")),
            Ok(Ok(handle)) => Ok(Conn { handle }),
        }
    }
}

/// The key files to try: the one `identity` names, or the first default that exists.
fn identity_files(settings: &Settings) -> Vec<std::path::PathBuf> {
    if let Some(path) = &settings.identity {
        return vec![path.clone()];
    }
    let Some(ssh) = home().map(|home| home.join(".ssh")) else {
        return Vec::new();
    };
    ["id_ed25519", "id_ecdsa", "id_rsa"]
        .into_iter()
        .map(|name| ssh.join(name))
        .filter(|path| path.is_file())
        .take(1)
        .collect()
}

/// Logs in with the agent's keys, then the key files, then the password; the error names each way that was tried.
async fn login(handle: &mut Handle<Client>, settings: &Settings) -> Result<(), String> {
    let user = settings.user.as_str();
    let mut tried: Vec<String> = Vec::new();
    let rsa_hash = handle
        .best_supported_rsa_hash()
        .await
        .ok()
        .flatten()
        .flatten();
    if settings.agent {
        match crate::agent::login(handle, user, rsa_hash).await {
            Ok(true) => return Ok(()),
            Ok(false) => tried.push("the agent's keys were refused".to_owned()),
            Err(why) => tried.push(format!("agent: {why}")),
        }
    }
    for path in identity_files(settings) {
        match load_secret_key(&path, settings.passphrase.as_deref()) {
            Ok(key) => {
                let hash = matches!(key.algorithm(), Algorithm::Rsa { .. })
                    .then_some(rsa_hash)
                    .flatten();
                match handle
                    .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                    .await
                {
                    Ok(result) if result.success() => return Ok(()),
                    Ok(_) => tried.push(format!("the host refused the key {}", path.display())),
                    Err(error) => tried.push(format!("{}: {error}", path.display())),
                }
            }
            Err(russh::keys::Error::KeyIsEncrypted) => tried.push(format!(
                "{} is encrypted and no passphrase is set",
                path.display()
            )),
            Err(error) => tried.push(format!("{}: {error}", path.display())),
        }
    }
    if let Some(password) = &settings.password {
        match handle.authenticate_password(user, password.clone()).await {
            Ok(result) if result.success() => return Ok(()),
            Ok(_) => tried.push("the host refused the password".to_owned()),
            Err(error) => tried.push(format!("password: {error}")),
        }
    }
    if tried.is_empty() {
        return Err(format!(
            "no way to log in as {user:?}: no agent key, no key file and no password"
        ));
    }
    Err(format!("login as {user:?} failed ({})", tried.join("; ")))
}

/// Runs `future` on `link`'s runtime for at most `limit`, from a thread that is not the runtime's.
pub(crate) fn within<F: Future>(
    link: &Link,
    limit: Duration,
    future: F,
) -> Result<F::Output, ProviderError> {
    link.rt
        .block_on(async { tokio::time::timeout(limit, future).await })
        .map_err(|_| {
            refuse(
                Status::GUATIAO_ERR_GONE,
                format!("{}: the host did not answer in time", link.target()),
            )
        })
}
