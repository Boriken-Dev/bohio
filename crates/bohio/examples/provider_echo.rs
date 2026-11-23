//! The provider `docs/providers.md` writes, compiled: an echo console, a terminal source whose far end answers what is typed.
//!
//! `provider_echo --listen tcp://127.0.0.1:0 --peer sha256:... [--seconds N]` serves a node whose only terminal source is the
//! echo, grants the peers it was given `info`, `shell` and `session`, prints `id <peer id>` and `listening <endpoint>` and runs
//! until it is killed or `--seconds` have passed. Then `bohio remote ENDPOINT --server ID console` reaches it, and what you type
//! comes back with `echo: ` in front.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use bohio::access::{Capability, Sources};
use bohio::config::{ListenConfig, NodeConfig};
use bohio::listen::Listening;
use bohio::node::{MemoryGrants, MemoryKeys, Node};
use bohio::policy::{Grant, Permission};
use bohio_api::guatiao::library::{Object, ProviderError};
use bohio_api::model::{SourceInfo, TerminalExit, TerminalOpen};
use bohio_api::{Terminal, TerminalSource, unavailable, unsupported, vocab};
use coqui_link::auth::{Identity, Method};
use coqui_peer::PeerId;

/// What the far end says and has not been read yet.
#[derive(Default)]
struct Line {
    heard: Mutex<VecDeque<u8>>,
    arrived: Condvar,
}

/// A source that is its own far end: it has no setting, no device and nothing to open.
struct Echo {
    line: Arc<Line>,
}

impl TerminalSource for Echo {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        // A console says honestly what it cannot do: a peer reads it from `info` before it asks.
        Ok(SourceInfo::new(
            "echo",
            &[vocab::terminal::READ, vocab::terminal::WRITE],
            &[
                vocab::terminal::RESIZE,
                vocab::terminal::SIGNALS,
                vocab::terminal::EXIT_STATUS,
                vocab::terminal::PROGRAMS,
            ],
        ))
    }

    fn open(&self, request: TerminalOpen) -> Result<Object<dyn Terminal>, ProviderError> {
        if request.program.is_some() {
            return Err(unsupported("the echo console starts no program"));
        }
        let greeting = b"echo console ready\r\n";
        self.line
            .heard
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(greeting);
        Ok(Session {
            line: Arc::clone(&self.line),
            closed: AtomicBool::new(false),
        }
        .into_object())
    }
}

struct Session {
    line: Arc<Line>,
    closed: AtomicBool,
}

impl Terminal for Session {
    fn read(&self, dst: &mut [u8], wait_ms: u64) -> Result<i64, ProviderError> {
        let end = Instant::now() + Duration::from_millis(wait_ms);
        let mut heard = self
            .line
            .heard
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            if self.closed.load(Ordering::Acquire) {
                return Ok(-1);
            }
            if !heard.is_empty() && !dst.is_empty() {
                let count = dst.len().min(heard.len());
                for (slot, byte) in dst.iter_mut().zip(heard.drain(..count)) {
                    *slot = byte;
                }
                return Ok(i64::try_from(count).unwrap_or(i64::MAX));
            }
            let now = Instant::now();
            if now >= end {
                return Ok(0);
            }
            heard = self
                .line
                .arrived
                .wait_timeout(heard, end - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn write(&self, data: &[u8]) -> Result<u64, ProviderError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(unavailable("the echo console was let go"));
        }
        let mut heard = self
            .line
            .heard
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        heard.extend(b"echo: ");
        heard.extend(data);
        self.line.arrived.notify_all();
        Ok(data.len() as u64)
    }

    fn kill(&self) -> Result<TerminalExit, ProviderError> {
        self.closed.store(true, Ordering::Release);
        self.line.arrived.notify_all();
        Ok(TerminalExit::default())
    }

    fn exit(&self) -> Result<Option<TerminalExit>, ProviderError> {
        Ok(None)
    }
}

fn main() -> Result<(), String> {
    let mut listen = "tcp://127.0.0.1:0".to_owned();
    let mut peers = Vec::new();
    let mut seconds: Option<u64> = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--listen" => listen = value()?,
            "--peer" => peers.push(value()?.parse::<PeerId>().map_err(|_| "a peer id")?),
            "--seconds" => seconds = Some(value()?.parse().map_err(|_| "--seconds is a number")?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let config = NodeConfig::default();
    let identity = Identity::generate(Method::DEFAULT).map_err(|error| error.to_string())?;
    let id = identity.peer_id();
    let keys = Arc::new(MemoryKeys::new());
    let grants = Arc::new(MemoryGrants::new());
    for peer in &peers {
        keys.accept(*peer);
        grants.set(
            *peer,
            Grant::allowing([Permission::Info, Permission::Shell, Permission::Session]),
        );
    }
    // The echo is the node's only terminal source: registered by name, made the default, no other served.
    let sources = Sources::unconfigured()
        .with_terminal(
            "echo",
            Arc::new(Echo {
                line: Arc::new(Line::default()),
            }),
        )
        .with_access(Capability::Terminal, ["echo"])
        .with_access(Capability::Desktop, []);
    let node = Node::new(config.clone(), identity, keys, grants)
        .map_err(|error| error.to_string())?
        .with_sources(sources);
    let entry = ListenConfig {
        endpoint: listen,
        ..ListenConfig::default()
    };
    let (listening, bound) =
        Listening::bind(&entry, &config.link.settings()).map_err(|error| error.to_string())?;
    println!("id {id}");
    println!("listening {bound}");
    std::io::stdout()
        .flush()
        .map_err(|error| error.to_string())?;
    let stop = Arc::new(AtomicBool::new(false));
    if let Some(seconds) = seconds {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(seconds));
            stop.store(true, Ordering::SeqCst);
        });
    }
    node.serve_listening(&listening, &stop)
        .map_err(|error| error.to_string())
}
