// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! An echo console as a provider library, for the tests of libraries a node runs in a process of their own.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bohio_api::guatiao::Provider;
use bohio_api::guatiao::library::{Object, ProviderError};
use bohio_api::model::{SourceInfo, TerminalExit, TerminalOpen};
use bohio_api::{Terminal, TerminalSource, unavailable, unsupported, vocab};

#[derive(Default)]
struct Line {
    heard: Mutex<VecDeque<u8>>,
    arrived: Condvar,
}

/// A console that is its own far end; what is typed comes back after `library-echo: `.
#[derive(Default, Provider)]
#[provider(
    TerminalSource,
    id = "library-echo",
    name = "echo console library",
    crate = ::bohio_api::guatiao
)]
pub struct Echo {
    line: Arc<Line>,
}

impl TerminalSource for Echo {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        Ok(SourceInfo::new(
            "library-echo",
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
            .extend(b"echo library ready\r\n");
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
        heard.extend(b"library-echo: ");
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

bohio_api::guatiao::providers!(Echo);
