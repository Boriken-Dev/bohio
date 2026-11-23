//! An in-process emulated path: a TCP proxy that holds every chunk for a one-way delay and sends it at a fixed
//! rate, in each direction, so a `local` run can measure a long, thin link on a machine with no network
//! namespaces. It adds the delay of `tc netem delay` and the rate of its `rate` and nothing else (no loss), and
//! its timing is the operating system's, so a result is a sanity check of what a change does on such a path.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// What a path does: each way takes `delay` and carries `rate` bytes a second (`0` is unlimited).
#[derive(Debug, Clone, Copy)]
pub struct Path {
    /// One-way delay.
    pub delay: Duration,
    /// Bytes a second each way.
    pub rate: u64,
}

impl Path {
    /// Parses `DELAY_MS` or `DELAY_MS:MBIT`, as `--wan 133:20` says the `wan` link of `netem.sh`.
    ///
    /// # Errors
    ///
    /// Text that is not one or two numbers.
    pub fn parse(text: &str) -> Result<Self, String> {
        let bad = || format!("`{text}` is not DELAY_MS or DELAY_MS:MBIT, such as 133:20");
        let (delay, rate) = text
            .split_once(':')
            .map_or((text, None), |(d, r)| (d, Some(r)));
        let delay: u64 = delay.parse().map_err(|_| bad())?;
        let rate = rate
            .map(|mbit| mbit.parse::<f64>().map_err(|_| bad()))
            .transpose()?
            .map_or(0, |mbit| (mbit * 1e6 / 8.0) as u64);
        Ok(Self {
            delay: Duration::from_millis(delay),
            rate,
        })
    }
}

/// Starts the proxy in front of `target` and returns the address clients dial. It runs until the process ends.
///
/// # Errors
///
/// The error of binding a loopback port.
pub fn start(target: SocketAddr, path: Path) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(client) = client else { continue };
            let Ok(server) = TcpStream::connect(target) else {
                continue;
            };
            let _ = client.set_nodelay(true);
            let _ = server.set_nodelay(true);
            if let (Ok(c2), Ok(s2)) = (client.try_clone(), server.try_clone()) {
                pump(client, s2, path);
                pump(server, c2, path);
            }
        }
    });
    Ok(address)
}

/// Moves what `from` reads to `to`, each chunk no sooner than `delay` after it was read and no faster than `rate`.
fn pump(mut from: TcpStream, mut to: TcpStream, path: Path) {
    let (send, receive) = mpsc::channel::<(Instant, Vec<u8>)>();
    thread::spawn(move || {
        let mut buffer = vec![0_u8; 16 * 1024];
        loop {
            match from.read(&mut buffer) {
                Ok(0) | Err(_) => return,
                Ok(count) => {
                    let due = Instant::now() + path.delay;
                    if send.send((due, buffer[..count].to_vec())).is_err() {
                        return;
                    }
                }
            }
        }
    });
    thread::spawn(move || {
        let mut free = Instant::now();
        while let Ok((due, bytes)) = receive.recv() {
            let start = due.max(free);
            let cost = if path.rate == 0 {
                Duration::ZERO
            } else {
                Duration::from_secs_f64(bytes.len() as f64 / path.rate as f64)
            };
            free = start + cost;
            if let Some(wait) = free.checked_duration_since(Instant::now()) {
                thread::sleep(wait);
            }
            if to.write_all(&bytes).is_err() {
                let _ = to.shutdown(std::net::Shutdown::Both);
                return;
            }
        }
        let _ = to.shutdown(std::net::Shutdown::Write);
    });
}
