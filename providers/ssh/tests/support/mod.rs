//! An in-process SSH server for tests: public-key and password login, exec and pty channels that run `sh -c`, a
//! limit on the sessions of a connection, and a record of what clients asked for.

#![allow(dead_code)]

mod sftp;

pub use sftp::Advertise;

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use bohio_provider_ssh::SshSettings;
use russh::keys::ssh_key::{LineEnding, PublicKey};
use russh::keys::{Algorithm, HashAlg, PrivateKey};
use russh::server::{Auth, ChannelOpenHandle, Msg, Server as _, Session, VerifiedPublicKey};
use russh::{Channel, ChannelId, Sig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

/// The user name the fake server accepts.
pub const USER: &str = "tester";
/// The password the fake server accepts when it takes one.
pub const PASSWORD: &str = "open-sesame";

/// What the fake server saw clients ask for.
#[derive(Default, Debug)]
pub struct Seen {
    /// `(columns, rows)` of each pty request.
    pub ptys: Vec<(u32, u32)>,
    /// `(columns, rows)` of each window change.
    pub window_changes: Vec<(u32, u32)>,
    /// Signal names clients sent.
    pub signals: Vec<String>,
    /// Commands clients asked to execute, and `<shell>` for each shell request.
    pub commands: Vec<String>,
    /// How many connections were accepted.
    pub connections: usize,
    /// The most session channels one connection had open at once.
    pub most_open: usize,
}

/// How the fake server behaves.
#[derive(Clone)]
pub struct Options {
    /// Session channels one connection may have open; the next one is refused.
    pub max_sessions: usize,
    /// Whether password login is accepted.
    pub password: bool,
    /// Whether public-key login is accepted.
    pub keys: bool,
    /// What the SFTP subsystem advertises; `None` serves no SFTP subsystem.
    pub sftp: Option<Advertise>,
    /// Start the program of a command line the provider built (`cd DIR && exec env ... PROGRAM ARGS`) directly, with no shell:
    /// for a machine that has no `sh`. A command of another shape is refused.
    pub direct: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_sessions: 100,
            password: false,
            keys: true,
            sftp: Some(Advertise::default()),
            direct: false,
        }
    }
}

pub struct FakeSsh {
    pub port: u16,
    pub seen: Arc<Mutex<Seen>>,
    pub dir: tempfile::TempDir,
    host_key: PrivateKey,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    handles: Arc<Mutex<Vec<russh::server::Handle>>>,
}

/// Whether the commands the fake server runs have a shell to run in.
pub fn shell_available() -> bool {
    std::process::Command::new("sh")
        .args(["-c", "exit 0"])
        .status()
        .is_ok_and(|status| status.success())
}

impl FakeSsh {
    /// Starts a server on a free loopback port. The client key is written to `<dir>/client_key`.
    pub fn start() -> Self {
        Self::launch(0, Algorithm::Ed25519, None, Options::default())
            .expect("a loopback port is free")
    }

    pub fn start_with(options: Options) -> Self {
        Self::launch(0, Algorithm::Ed25519, None, options).expect("a loopback port is free")
    }

    /// Starts a server on `port` that accepts the key in `client_key`, or answers `None` when the port cannot be
    /// bound.
    pub fn start_on(port: u16, client_key: &std::path::Path) -> Option<Self> {
        Self::launch(
            port,
            Algorithm::Ed25519,
            Some(client_key),
            Options::default(),
        )
    }

    fn launch(
        port: u16,
        host_algorithm: Algorithm,
        existing: Option<&std::path::Path>,
        options: Options,
    ) -> Option<Self> {
        let dir = tempfile::tempdir().unwrap();
        let client = match existing {
            Some(path) => russh::keys::load_secret_key(path, None).unwrap(),
            None => PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519).unwrap(),
        };
        client
            .write_openssh_file(dir.path().join("client_key"), LineEnding::LF)
            .unwrap();
        let host_key = PrivateKey::random(&mut rand::rng(), host_algorithm).unwrap();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let (shutdown, stop) = oneshot::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();

        let config = Arc::new(russh::server::Config {
            auth_rejection_time: std::time::Duration::from_millis(10),
            auth_rejection_time_initial: Some(std::time::Duration::ZERO),
            keys: vec![host_key.clone()],
            inactivity_timeout: None,
            ..russh::server::Config::default()
        });
        let authorized = client.public_key().clone();
        let thread_seen = seen.clone();
        let handles = Arc::new(Mutex::new(Vec::new()));
        let thread_handles = handles.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let Ok(listener) = TcpListener::bind(("127.0.0.1", port)).await else {
                    ready_tx.send(None).unwrap();
                    return;
                };
                ready_tx.send(listener.local_addr().ok()).unwrap();
                let mut server = Server {
                    authorized,
                    seen: thread_seen,
                    handles: thread_handles,
                    options,
                    modes: Arc::default(),
                };
                tokio::select! {
                    _ = stop => {}
                    _ = async {
                        loop {
                            let Ok((socket, peer)) = listener.accept().await else { break };
                            let handler = server.new_client(Some(peer));
                            let config = config.clone();
                            tokio::spawn(async move {
                                if let Ok(session) = russh::server::run_stream(config, socket, handler).await {
                                    let _ = session.await;
                                }
                            });
                        }
                    } => {}
                }
            });
            runtime.shutdown_background();
        });
        let Some(address) = ready_rx.recv().unwrap() else {
            let _ = thread.join();
            return None;
        };
        Some(Self {
            port: address.port(),
            seen,
            dir,
            host_key,
            shutdown: Some(shutdown),
            thread: Some(thread),
            handles,
        })
    }

    /// Closes every open connection from the server's side.
    pub fn disconnect_all(&self) {
        let handles: Vec<_> = self.handles.lock().unwrap().drain(..).collect();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        for handle in handles {
            let _ = runtime.block_on(handle.disconnect(
                russh::Disconnect::ByApplication,
                "test".to_owned(),
                "en".to_owned(),
            ));
        }
    }

    /// The server's host key as `sha256:HEX`.
    pub fn pin(&self) -> String {
        let fingerprint = self.host_key.public_key().fingerprint(HashAlg::Sha256);
        let mut pin = String::from("sha256:");
        for byte in fingerprint.as_bytes() {
            pin.push_str(&format!("{byte:02x}"));
        }
        pin
    }

    /// A `known_hosts` line that holds the server's key.
    pub fn known_hosts_line(&self) -> String {
        let key = self.host_key.public_key().to_openssh().unwrap();
        format!("[127.0.0.1]:{} {key}\n", self.port)
    }

    /// The path of the private key the server accepts.
    pub fn identity(&self) -> PathBuf {
        self.dir.path().join("client_key")
    }

    /// Settings that pin the host key and log in with the key file, with no agent.
    pub fn settings(&self) -> SshSettings {
        let mut settings = self.unpinned_settings();
        settings.host_key = Some(self.pin());
        settings
    }

    /// Settings with no pin (the host key check is `strict` and `known_hosts` is a file that does not exist).
    pub fn unpinned_settings(&self) -> SshSettings {
        let mut settings = SshSettings::new("127.0.0.1");
        settings.port = u64::from(self.port);
        settings.user = Some(USER.to_owned());
        settings.identity = Some(self.identity().to_string_lossy().into_owned());
        settings.known_hosts = Some(
            self.dir
                .path()
                .join("no_known_hosts")
                .to_string_lossy()
                .into_owned(),
        );
        settings.agent = false;
        settings.reconnect_after = "0s".to_owned();
        settings
    }
}

impl Drop for FakeSsh {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone)]
struct Server {
    authorized: PublicKey,
    seen: Arc<Mutex<Seen>>,
    handles: Arc<Mutex<Vec<russh::server::Handle>>>,
    options: Options,
    modes: Arc<Mutex<HashMap<PathBuf, u32>>>,
}

impl russh::server::Server for Server {
    type Handler = Connection;

    fn new_client(&mut self, _: Option<SocketAddr>) -> Connection {
        self.seen.lock().unwrap().connections += 1;
        Connection {
            handles: self.handles.clone(),
            authorized: self.authorized.clone(),
            seen: self.seen.clone(),
            options: self.options.clone(),
            modes: self.modes.clone(),
            channels: HashMap::new(),
            ptys: HashSet::new(),
            stdin: HashMap::new(),
            kill: HashMap::new(),
            open: 0,
        }
    }
}

struct Connection {
    handles: Arc<Mutex<Vec<russh::server::Handle>>>,
    authorized: PublicKey,
    seen: Arc<Mutex<Seen>>,
    options: Options,
    modes: Arc<Mutex<HashMap<PathBuf, u32>>>,
    channels: HashMap<ChannelId, Channel<Msg>>,
    ptys: HashSet<ChannelId>,
    stdin: HashMap<ChannelId, ChildStdin>,
    kill: HashMap<ChannelId, oneshot::Sender<()>>,
    open: usize,
}

impl russh::server::Handler for Connection {
    type Error = russh::Error;

    async fn auth_publickey(
        &mut self,
        user: &str,
        key: VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        if self.options.keys && user == USER && *key.public_key == self.authorized {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if self.options.password && user == USER && password == PASSWORD {
            Ok(Auth::Accept)
        } else {
            Ok(Auth::reject())
        }
    }

    async fn auth_succeeded(&mut self, session: &mut Session) -> Result<(), Self::Error> {
        self.handles.lock().unwrap().push(session.handle());
        Ok(())
    }

    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if self.open >= self.options.max_sessions {
            // Dropping the reply refuses the channel, as sshd's `MaxSessions` does.
            return Ok(());
        }
        self.open += 1;
        {
            let mut seen = self.seen.lock().unwrap();
            seen.most_open = seen.most_open.max(self.open);
        }
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(russh::Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.ptys.insert(channel);
        self.seen.lock().unwrap().ptys.push((col_width, row_height));
        session.channel_success(channel)
    }

    async fn window_change_request(
        &mut self,
        _channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.seen
            .lock()
            .unwrap()
            .window_changes
            .push((col_width, row_height));
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.seen
            .lock()
            .unwrap()
            .commands
            .push("<shell>".to_owned());
        self.run(channel, "sh".to_owned(), session)
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        self.seen.lock().unwrap().commands.push(command.clone());
        self.run(channel, command, session)
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        if let Some(stdin) = self.stdin.get_mut(&channel) {
            let _ = stdin.write_all(data).await;
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.stdin.remove(&channel);
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.stdin.remove(&channel);
        if self.channels.remove(&channel).is_some() {
            self.open = self.open.saturating_sub(1);
        }
        if let Some(kill) = self.kill.remove(&channel) {
            let _ = kill.send(());
        }
        Ok(())
    }

    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let (Some(advertise), "sftp") = (self.options.sftp, name) else {
            return session.channel_failure(channel);
        };
        let Some(stream) = self.channels.remove(&channel) else {
            return session.channel_failure(channel);
        };
        session.channel_success(channel)?;
        tokio::spawn(russh_sftp::server::run(
            stream.into_stream(),
            sftp::Sftp::new(advertise, self.modes.clone()),
        ));
        Ok(())
    }

    async fn signal(
        &mut self,
        channel: ChannelId,
        signal: Sig,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.seen
            .lock()
            .unwrap()
            .signals
            .push(format!("{signal:?}"));
        if let Some(kill) = self.kill.remove(&channel) {
            let _ = kill.send(());
        }
        Ok(())
    }
}

impl Connection {
    /// Runs `command` under `sh -c` for the channel, with its output relayed and its exit reported.
    fn run(
        &mut self,
        channel: ChannelId,
        command: String,
        session: &mut Session,
    ) -> Result<(), russh::Error> {
        let mut process = if self.options.direct {
            let Some(line) = parse_line(&command) else {
                return session.channel_failure(channel);
            };
            let mut process = Command::new(&line.argv[0]);
            process.args(&line.argv[1..]);
            if let Some(cwd) = &line.cwd {
                process.current_dir(cwd);
            }
            if line.clear_env {
                process.env_clear();
            }
            for (name, value) in &line.env {
                match value {
                    Some(value) => process.env(name, value),
                    None => process.env_remove(name),
                };
            }
            process
        } else {
            let mut process = Command::new("sh");
            process.arg("-c").arg(&command);
            process
        };
        let spawned = process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn();
        let Ok(mut child) = spawned else {
            return session.channel_failure(channel);
        };
        session.channel_success(channel)?;
        if let Some(stdin) = child.stdin.take() {
            self.stdin.insert(channel, stdin);
        }
        let (kill, killed) = oneshot::channel();
        self.kill.insert(channel, kill);
        let merge = self.ptys.contains(&channel);
        tokio::spawn(supervise(session.handle(), channel, child, merge, killed));
        Ok(())
    }
}

/// A command line of the provider's shape, taken apart.
#[derive(Debug, PartialEq, Eq)]
pub struct Line {
    pub cwd: Option<String>,
    pub clear_env: bool,
    /// `(name, Some(value))` to set, `(name, None)` to remove.
    pub env: Vec<(String, Option<String>)>,
    pub argv: Vec<String>,
}

/// The words of a POSIX command line made of single-quoted words (`'it'\''s'` is one) and the unquoted `&&`.
fn words(text: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match (quoted, c) {
            (true, '\'') => quoted = false,
            (true, c) => current.push(c),
            (false, '\'') => {
                quoted = true;
                started = true;
            }
            (false, '\\') => {
                current.push(chars.next()?);
                started = true;
            }
            (false, c) if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            (false, c) => {
                current.push(c);
                started = true;
            }
        }
    }
    if quoted {
        return None;
    }
    if started {
        out.push(current);
    }
    Some(out)
}

/// `cd 'DIR' && exec env [-i] [-u 'NAME']... ['NAME=VALUE']... 'PROGRAM' 'ARG'...`, or the same without `cd` or `env`.
pub fn parse_line(command: &str) -> Option<Line> {
    let mut words = words(command)?.into_iter().peekable();
    let mut line = Line {
        cwd: None,
        clear_env: false,
        env: Vec::new(),
        argv: Vec::new(),
    };
    if words.peek().map(String::as_str) == Some("cd") {
        words.next();
        line.cwd = Some(words.next()?);
        if words.next()? != "&&" {
            return None;
        }
    }
    if words.next()? != "exec" {
        return None;
    }
    if words.peek().map(String::as_str) == Some("env") {
        words.next();
        loop {
            match words.peek().map(String::as_str) {
                Some("-i") => {
                    words.next();
                    line.clear_env = true;
                }
                Some("-u") => {
                    words.next();
                    line.env.push((words.next()?, None));
                }
                Some(word) if word.contains('=') && !word.starts_with('=') => {
                    let (name, value) = word.split_once('=')?;
                    line.env.push((name.to_owned(), Some(value.to_owned())));
                    words.next();
                }
                _ => break,
            }
        }
    }
    line.argv = words.collect();
    (!line.argv.is_empty()).then_some(line)
}

/// Relays a child's output to the channel, then its exit status.
async fn supervise(
    handle: russh::server::Handle,
    channel: ChannelId,
    mut child: Child,
    merge: bool,
    mut killed: oneshot::Receiver<()>,
) {
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let (mut out_buf, mut err_buf) = ([0u8; 4096], [0u8; 4096]);
    let (mut out_open, mut err_open) = (true, true);
    while out_open || err_open {
        tokio::select! {
            read = stdout.read(&mut out_buf), if out_open => match read {
                Ok(0) | Err(_) => out_open = false,
                Ok(n) => { let _ = handle.data(channel, out_buf[..n].to_vec()).await; }
            },
            read = stderr.read(&mut err_buf), if err_open => match read {
                Ok(0) | Err(_) => err_open = false,
                Ok(n) => {
                    let bytes = err_buf[..n].to_vec();
                    let _ = if merge {
                        handle.data(channel, bytes).await.map_err(|_| ())
                    } else {
                        handle.extended_data(channel, 1, bytes).await.map_err(|_| ())
                    };
                }
            },
            _ = &mut killed => {
                let _ = child.kill().await;
                break;
            }
        }
    }
    let code = match child.wait().await {
        Ok(status) => status.code().map_or(255, |code| code as u32),
        Err(_) => 255,
    };
    let _ = handle.exit_status_request(channel, code).await;
    let _ = handle.eof(channel).await;
    let _ = handle.close(channel).await;
}
