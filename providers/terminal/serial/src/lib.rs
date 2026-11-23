// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `bohio-provider-serial`: the console of a system reached through a serial line, as a Bohío terminal source.
//!
//! The line is the console: nothing is started, what the far end writes is what a peer reads and what a peer types is
//! what the far end reads. A line has no size, no signals and no exit status, and says so in `describe`. The port is
//! opened when the source is first used and opened again `reopen_after` after it vanishes (an unplugged USB adapter, a
//! device that reset), so a console outlives the cable.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use bohio_api::guatiao::library::{Object, ProviderError};
use bohio_api::guatiao::{FromValue, Provider, Schema, Status, ToValue};
use bohio_api::model::{SourceInfo, TerminalExit, TerminalOpen};
use bohio_api::{Terminal, TerminalSource, unavailable, vocab};
use coqui_transport::Split;
use coqui_transport_serial::{
    DataBits, FlowControl, Parity, ReadHalf, SerialBuilder, StopBits, WriteHalf,
};

/// `[source.<name>]` of a serial console: the line and how long to wait for it. Every key but `path` is optional.
#[derive(ToValue, FromValue, Schema, Debug, Clone, PartialEq, Eq)]
#[map(crate = ::bohio_api::guatiao)]
pub struct SerialConfig {
    /// The device: `/dev/ttyUSB0` on Unix, `COM3` on Windows (`\\.\COM10` above 9).
    pub path: String,
    /// Bits a second. Above 0. Default `115200`.
    #[map(default = 115_200)]
    pub baud_rate: u64,
    /// Data bits, 5 to 8. Default `8`.
    #[map(default = 8)]
    pub data_bits: u64,
    /// `none`, `odd` or `even`. Default `none`.
    #[map(default = String::from("none"))]
    pub parity: String,
    /// `1` or `2`. Default `1`.
    #[map(default = 1)]
    pub stop_bits: u64,
    /// `none`, `software` (XON/XOFF) or `hardware` (RTS/CTS). Default `none`.
    #[map(default = String::from("none"))]
    pub flow_control: String,
    /// The DTR line when the port opens; unchanged when absent. Many devices reset when it changes.
    pub dtr: Option<bool>,
    /// The RTS line when the port opens; unchanged when absent.
    pub rts: Option<bool>,
    /// How long after the port vanishes (and after a failed open) it is opened again, and how long `describe` waits for
    /// the first open (`1s`, `500ms`). Not zero. Default `1s`.
    #[map(default = String::from("1s"))]
    pub reopen_after: String,
    /// Bytes read from the line and not yet taken by a peer; the reader waits when they are full, so the driver's buffer
    /// and the line's flow control hold back what comes next. At least 1. Default `65536`.
    #[map(default = 65_536)]
    pub queue_bytes: u64,
}

/// A time: a number and `ms`, `s`, `m` or `h`; a bare number is seconds.
fn span(text: &str) -> Option<Duration> {
    let text = text.trim();
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    let (number, unit) = text.split_at(digits);
    let number: u64 = number.parse().ok()?;
    match unit.trim() {
        "ms" => Some(Duration::from_millis(number)),
        "" | "s" => Some(Duration::from_secs(number)),
        "m" => Some(Duration::from_secs(number.checked_mul(60)?)),
        "h" => Some(Duration::from_secs(number.checked_mul(3600)?)),
        _ => None,
    }
}

fn refuse(message: &str) -> ProviderError {
    ProviderError::new(Status::GUATIAO_ERR_BAD_VALUE, message)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the device's thread and the terminals share.
struct Shared {
    config: SerialConfig,
    reopen: Duration,
    capacity: usize,
    queue: Mutex<VecDeque<u8>>,
    arrived: Condvar,
    space: Condvar,
    writer: Mutex<Option<WriteHalf>>,
    /// `Ok` while the port is open, else why it is not.
    status: Mutex<Result<(), String>>,
    /// The first open has been tried.
    tried: AtomicBool,
    changed: Condvar,
    stop: AtomicBool,
}

impl Shared {
    fn open(&self) -> Result<(ReadHalf, WriteHalf), String> {
        let config = &self.config;
        let data_bits = match config.data_bits {
            5 => DataBits::Five,
            6 => DataBits::Six,
            7 => DataBits::Seven,
            _ => DataBits::Eight,
        };
        let parity = match config.parity.as_str() {
            "odd" => Parity::Odd,
            "even" => Parity::Even,
            _ => Parity::None,
        };
        let stop_bits = if config.stop_bits == 2 {
            StopBits::Two
        } else {
            StopBits::One
        };
        let flow = match config.flow_control.as_str() {
            "software" => FlowControl::Software,
            "hardware" => FlowControl::Hardware,
            _ => FlowControl::None,
        };
        let port = SerialBuilder::new()
            .path(&config.path)
            .baud_rate(u32::try_from(config.baud_rate).unwrap_or(u32::MAX))
            .data_bits(data_bits)
            .parity(parity)
            .stop_bits(stop_bits)
            .flow_control(flow)
            .dtr(config.dtr)
            .rts(config.rts)
            .build()
            .map_err(|error| error.to_string())?;
        let halves = port.split().map_err(|error| error.to_string())?;
        Ok((halves.reader, halves.writer))
    }

    fn set_status(&self, status: Result<(), String>) {
        *lock(&self.status) = status;
        self.tried.store(true, Ordering::Release);
        self.changed.notify_all();
    }

    /// Reads the line into the queue until it fails or the source goes.
    fn pump(&self, mut reader: ReadHalf) {
        let mut buffer = vec![0_u8; 4096];
        while !self.stop.load(Ordering::Acquire) {
            match reader.read(&mut buffer) {
                Ok(0) => return,
                Ok(count) => {
                    let mut rest = &buffer[..count];
                    let mut queue = lock(&self.queue);
                    while !rest.is_empty() {
                        if self.stop.load(Ordering::Acquire) {
                            return;
                        }
                        let room = self.capacity.saturating_sub(queue.len());
                        if room == 0 {
                            queue = self
                                .space
                                .wait_timeout(queue, Duration::from_millis(100))
                                .unwrap_or_else(PoisonError::into_inner)
                                .0;
                            continue;
                        }
                        let (now, later) = rest.split_at(room.min(rest.len()));
                        queue.extend(now.iter().copied());
                        rest = later;
                        self.arrived.notify_all();
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted
                    ) => {}
                Err(_) => return,
            }
        }
    }

    /// The device's thread: opens the line, reads it, and opens it again when it goes.
    fn run(self: &Arc<Self>) {
        while !self.stop.load(Ordering::Acquire) {
            match self.open() {
                Ok((reader, writer)) => {
                    *lock(&self.writer) = Some(writer);
                    self.set_status(Ok(()));
                    self.pump(reader);
                    *lock(&self.writer) = None;
                    self.set_status(Err("the line closed".to_owned()));
                }
                Err(reason) => self.set_status(Err(reason)),
            }
            let until = Instant::now() + self.reopen;
            while Instant::now() < until && !self.stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(20).min(self.reopen));
            }
        }
    }
}

/// A serial console source.
#[derive(Default, Provider)]
#[provider(
    TerminalSource,
    id = "serial",
    name = "serial console",
    crate = ::bohio_api::guatiao,
    config = SerialConfig
)]
pub struct Serial {
    shared: Option<Arc<Shared>>,
    started: AtomicBool,
}

impl TryFrom<SerialConfig> for Serial {
    type Error = ProviderError;

    fn try_from(config: SerialConfig) -> Result<Self, Self::Error> {
        if config.path.is_empty() {
            return Err(refuse("path names no device"));
        }
        if config.baud_rate == 0 {
            return Err(refuse("baud_rate is 0"));
        }
        if !(5..=8).contains(&config.data_bits) {
            return Err(refuse("data_bits is 5, 6, 7 or 8"));
        }
        if !["none", "odd", "even"].contains(&config.parity.as_str()) {
            return Err(refuse("parity is none, odd or even"));
        }
        if ![1, 2].contains(&config.stop_bits) {
            return Err(refuse("stop_bits is 1 or 2"));
        }
        if !["none", "software", "hardware"].contains(&config.flow_control.as_str()) {
            return Err(refuse("flow_control is none, software or hardware"));
        }
        let reopen = span(&config.reopen_after)
            .filter(|reopen| !reopen.is_zero())
            .ok_or_else(|| refuse("reopen_after is a time above zero, such as 1s or 500ms"))?;
        if config.queue_bytes == 0 {
            return Err(refuse(
                "queue_bytes is 0: nothing read from the line has room",
            ));
        }
        let capacity = usize::try_from(config.queue_bytes).unwrap_or(usize::MAX);
        Ok(Self {
            shared: Some(Arc::new(Shared {
                config,
                reopen,
                capacity,
                queue: Mutex::new(VecDeque::new()),
                arrived: Condvar::new(),
                space: Condvar::new(),
                writer: Mutex::new(None),
                status: Mutex::new(Err("the line has not been opened yet".to_owned())),
                tried: AtomicBool::new(false),
                changed: Condvar::new(),
                stop: AtomicBool::new(false),
            })),
            started: AtomicBool::new(false),
        })
    }
}

impl Serial {
    fn shared(&self) -> Result<&Arc<Shared>, ProviderError> {
        self.shared
            .as_ref()
            .ok_or_else(|| ProviderError::new(Status::GUATIAO_ERR_NULL, "no line is configured"))
    }

    /// Starts the device's thread the first time the source is used, and waits for its first try at the line.
    fn started(&self) -> Result<&Arc<Shared>, ProviderError> {
        let shared = self.shared()?;
        if !self.started.swap(true, Ordering::AcqRel) {
            let device = Arc::clone(shared);
            thread::Builder::new()
                .name("bohio-serial-console".to_owned())
                .spawn(move || device.run())
                .map_err(|_| {
                    ProviderError::new(
                        Status::GUATIAO_ERR_INTERNAL,
                        "a thread could not be started",
                    )
                })?;
        }
        let until = Instant::now() + shared.reopen;
        let mut status = lock(&shared.status);
        while !shared.tried.load(Ordering::Acquire) && Instant::now() < until {
            status = shared
                .changed
                .wait_timeout(status, Duration::from_millis(20))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        drop(status);
        Ok(shared)
    }

    fn reason(shared: &Shared) -> Option<String> {
        lock(&shared.status).clone().err()
    }
}

impl Drop for Serial {
    fn drop(&mut self) {
        if let Some(shared) = &self.shared {
            shared.stop.store(true, Ordering::Release);
            shared.space.notify_all();
            shared.arrived.notify_all();
        }
    }
}

impl TerminalSource for Serial {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        let shared = self.started()?;
        let info = SourceInfo::new(
            "serial",
            &[vocab::terminal::READ, vocab::terminal::WRITE],
            &[
                vocab::terminal::RESIZE,
                vocab::terminal::SIGNALS,
                vocab::terminal::EXIT_STATUS,
                vocab::terminal::PROGRAMS,
                vocab::terminal::BREAK,
            ],
        );
        Ok(match Self::reason(shared) {
            None => info,
            Some(reason) => info.unavailable(&reason),
        })
    }

    fn open(&self, request: TerminalOpen) -> Result<Object<dyn Terminal>, ProviderError> {
        if request.program.is_some() {
            return Err(refuse("a serial console starts no program"));
        }
        let shared = self.started()?;
        if let Some(reason) = Self::reason(shared) {
            return Err(unavailable(&reason));
        }
        Ok(Console {
            shared: Arc::clone(shared),
            closed: AtomicBool::new(false),
        }
        .into_object())
    }
}

/// One attach to the line.
struct Console {
    shared: Arc<Shared>,
    closed: AtomicBool,
}

impl Terminal for Console {
    fn read(&self, dst: &mut [u8], wait_ms: u64) -> Result<i64, ProviderError> {
        let end = Instant::now() + Duration::from_millis(wait_ms);
        let mut queue = lock(&self.shared.queue);
        loop {
            if self.closed.load(Ordering::Acquire) || self.shared.stop.load(Ordering::Acquire) {
                return Ok(-1);
            }
            if !queue.is_empty() && !dst.is_empty() {
                let count = dst.len().min(queue.len());
                for (slot, byte) in dst.iter_mut().zip(queue.drain(..count)) {
                    *slot = byte;
                }
                self.shared.space.notify_all();
                return Ok(i64::try_from(count).unwrap_or(i64::MAX));
            }
            let now = Instant::now();
            if now >= end {
                return Ok(0);
            }
            queue = self
                .shared
                .arrived
                .wait_timeout(queue, end - now)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn write(&self, data: &[u8]) -> Result<u64, ProviderError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(unavailable("the console was let go"));
        }
        let mut writer = lock(&self.shared.writer);
        let Some(writer) = writer.as_mut() else {
            return Err(unavailable("the line is not open"));
        };
        let mut rest = data;
        // The port reports a write that took nothing for its timeout as `TimedOut`; the line may be held by flow control.
        let mut stalls = 0_u32;
        while !rest.is_empty() {
            match writer.write(rest) {
                Ok(0) => return Err(unavailable("the line took no bytes")),
                Ok(count) => {
                    rest = &rest[count..];
                    stalls = 0;
                }
                Err(error)
                    if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) =>
                {
                    stalls += 1;
                    if stalls > 8 || self.closed.load(Ordering::Acquire) {
                        return Err(unavailable("the line took no bytes"));
                    }
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(_) => return Err(unavailable("the line failed")),
            }
        }
        let _ = writer.flush();
        Ok(data.len() as u64)
    }

    fn kill(&self) -> Result<TerminalExit, ProviderError> {
        // Letting go of the line is all there is to end: the far end's program is not ours.
        self.closed.store(true, Ordering::Release);
        self.shared.arrived.notify_all();
        Ok(TerminalExit::default())
    }

    fn exit(&self) -> Result<Option<TerminalExit>, ProviderError> {
        Ok(None)
    }
}

bohio_api::guatiao::local_providers!(
    id = "bohio-serial",
    version = env!("CARGO_PKG_VERSION"),
    providers = [Serial]
);

#[cfg(test)]
mod tests {
    use super::span;
    use std::time::Duration;

    #[test]
    fn a_time_is_a_number_and_a_unit() {
        assert_eq!(span("1s"), Some(Duration::from_secs(1)));
        assert_eq!(span("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(span("2"), Some(Duration::from_secs(2)));
        assert_eq!(span("1h"), Some(Duration::from_secs(3600)));
        assert_eq!(span("soon"), None);
    }
}
