//! A provider that is a program: an echo console served over guatiao's remote protocol on this process's standard input and
//! output, for a node that starts it with `[[providers.program]]`.
//!
//! ```toml
//! [[providers.program]]
//! command = ["/path/to/program_echo"]
//!
//! [source.echo]
//! provider = "program-echo"
//! ```
//!
//! What is typed comes back with `--prefix TEXT` (default `echo: `) in front. Typing `crash` ends this process, which is
//! how the tests show a provider that dies leaves the node standing. Nothing else may be written to standard output: it is the
//! link. `--silent` and `--exit N` make it a program that never speaks, or one that ends at once.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use bohio_api::guatiao::Provider;
use bohio_api::guatiao::library::{Object, ProviderError, Registry};
use bohio_api::model::{SourceInfo, TerminalExit, TerminalOpen};
use bohio_api::{Terminal, TerminalSource, unavailable, unsupported, vocab};
use guatiao_source_remote::transport::child;
use guatiao_source_remote::{Kinds, LinkOptions, Server, Trust};

/// What the far end says and has not been read yet.
#[derive(Default)]
struct Line {
    heard: Mutex<VecDeque<u8>>,
    arrived: Condvar,
}

/// What comes back in front of what was typed: `--prefix TEXT`, else `echo: `. A provider a program serves is one instance
/// that is configured where it runs, so its settings are the program's own arguments.
static PREFIX: OnceLock<String> = OnceLock::new();

/// A console that is its own far end.
#[derive(Default, Provider)]
#[provider(
    TerminalSource,
    id = "program-echo",
    name = "echo console program",
    crate = ::bohio_api::guatiao
)]
pub struct Echo {
    line: Arc<Line>,
}

impl TerminalSource for Echo {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        Ok(SourceInfo::new(
            "program-echo",
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
        self.line
            .heard
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend(b"echo console ready\r\n");
        Ok(Session {
            prefix: PREFIX.get().cloned().unwrap_or_else(|| "echo: ".to_owned()),
            line: Arc::clone(&self.line),
            closed: AtomicBool::new(false),
        }
        .into_object())
    }
}

struct Session {
    prefix: String,
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
        if data.windows(5).any(|window| window == b"crash") {
            std::process::exit(3);
        }
        let mut heard = self
            .line
            .heard
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        heard.extend(self.prefix.as_bytes());
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

bohio_api::guatiao::local_providers!(id = "program-echo", version = "0.0.0", providers = [Echo]);

fn main() -> Result<(), String> {
    // `--silent` never speaks and `--exit N` ends at once with code N: what the tests start to see a node's answer to either.
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--silent") => loop {
            std::thread::sleep(Duration::from_secs(3600));
        },
        Some("--exit") => {
            std::process::exit(args.next().and_then(|code| code.parse().ok()).unwrap_or(1));
        }
        Some("--prefix") => {
            let _ = PREFIX.set(args.next().unwrap_or_default());
        }
        _ => {}
    }
    let mut registry = Registry::new("program-echo", "0.0.0");
    registry
        .register_local("program-echo", library)
        .map_err(|error| error.to_string())?;
    let server = Server::new(registry, Kinds::new(), LinkOptions::new(Trust::Local));
    let link = server
        .serve(child::stdio())
        .map_err(|error| error.to_string())?;
    // The node closes this process's input when it is done with it, which ends the link.
    while link.alive() {
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}
