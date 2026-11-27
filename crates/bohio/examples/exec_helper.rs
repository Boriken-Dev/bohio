//! A small program the process-execution tests start, so they behave the same on every platform.
//!
//! `echo TEXT...`, `exit CODE`, `both` (writes both streams, exits 3), `cat`, `sleep MS`,
//! `tree MS` (starts a sleeping child and prints its id), `orphan MS` (starts a sleeping child
//! that keeps standard output open, then exits at once), `env NAME`, `cwd`, `whoami` (prints its own path), `flood BYTES`,
//! `count N MS` (prints 1 to N, a line every MS milliseconds), `trap` (prints `ready`, then waits; on an interrupt it
//! prints `got-int` and exits 3, and on a break or any other end it does not get to) and `read-line` (prints `ready`, then
//! prints `got:` and the first line of standard input).

#![forbid(unsafe_code)]
#![expect(
    clippy::zombie_processes,
    reason = "the helper starts children it must leave running"
)]

use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut out = std::io::stdout();
    match words.as_slice() {
        ["echo", rest @ ..] => {
            let _ = writeln!(out, "{}", rest.join(" "));
        }
        ["exit", code] => std::process::exit(code.parse().unwrap_or(1)),
        ["both"] => {
            let _ = write!(out, "out");
            let _ = out.flush();
            eprint!("err");
            std::process::exit(3);
        }
        ["cat"] => {
            let mut buffer = [0_u8; 4096];
            let mut input = std::io::stdin();
            while let Ok(count) = input.read(&mut buffer) {
                if count == 0 {
                    break;
                }
                let _ = out.write_all(&buffer[..count]);
                let _ = out.flush();
            }
        }
        ["sleep", ms] => std::thread::sleep(Duration::from_millis(ms.parse().unwrap_or(0))),
        ["count", n, ms] => {
            let pause = Duration::from_millis(ms.parse().unwrap_or(0));
            for line in 1..=n.parse::<u64>().unwrap_or(0) {
                if writeln!(out, "{line}").and_then(|()| out.flush()).is_err() {
                    break;
                }
                std::thread::sleep(pause);
            }
        }
        ["trap"] => {
            let (tx, rx) = std::sync::mpsc::channel::<()>();
            ctrlc::set_handler(move || {
                let _ = tx.send(());
            })
            .expect("the handler is installed");
            let _ = writeln!(out, "ready");
            let _ = out.flush();
            if rx.recv_timeout(Duration::from_secs(60)).is_ok() {
                let _ = writeln!(out, "got-int");
                let _ = out.flush();
                std::process::exit(3);
            }
        }
        ["read-line"] => {
            let _ = writeln!(out, "ready");
            let _ = out.flush();
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            let _ = write!(out, "got:{line}");
            let _ = out.flush();
        }
        ["tree", ms] => {
            let me = std::env::current_exe().expect("own path");
            let child = Command::new(me)
                .args(["sleep", ms])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("grandchild starts");
            let _ = writeln!(out, "{}", child.id());
            let _ = out.flush();
            std::thread::sleep(Duration::from_millis(ms.parse().unwrap_or(0)));
        }
        ["orphan", ms] => {
            let me = std::env::current_exe().expect("own path");
            let child = Command::new(me)
                .args(["sleep", ms])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("grandchild starts");
            let _ = writeln!(out, "{}", child.id());
            let _ = out.flush();
        }
        // An orphan that holds the output open, then `bytes` of output the reader may not take.
        ["orphan-flood", ms, bytes] => {
            let me = std::env::current_exe().expect("own path");
            let child = Command::new(me)
                .args(["sleep", ms])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("grandchild starts");
            let _ = writeln!(out, "{}", child.id());
            let _ = out.flush();
            let _ = out.write_all(&vec![b'x'; bytes.parse().unwrap_or(0)]);
        }
        ["env", name] => {
            let _ = writeln!(
                out,
                "{}",
                std::env::var(name).unwrap_or_else(|_| "<unset>".to_owned())
            );
        }
        ["whoami"] => {
            let _ = writeln!(
                out,
                "{}",
                std::env::current_exe()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            );
        }
        ["cwd"] => {
            let _ = writeln!(
                out,
                "{}",
                std::env::current_dir()
                    .map(|d| d.display().to_string())
                    .unwrap_or_default()
            );
        }
        ["flood", bytes] => {
            let mut left: usize = bytes.parse().unwrap_or(0);
            let block = [b'x'; 8192];
            while left > 0 {
                let take = left.min(block.len());
                if out.write_all(&block[..take]).is_err() {
                    break;
                }
                left -= take;
            }
        }
        // Varied text of the given length on standard output and, the same bytes, on standard error;
        // `noise` is pseudo-random bytes (incompressible) on standard output.
        ["text", bytes] => {
            let text = varied(bytes.parse().unwrap_or(0));
            let mut err = std::io::stderr();
            for part in text.chunks(1000) {
                let _ = out.write_all(part);
                let _ = out.flush();
                let _ = err.write_all(part);
                let _ = err.flush();
            }
        }
        ["noise", bytes] => {
            let mut state = 0x2545_F491_4F6C_DD1D_u64;
            let noise: Vec<u8> = (0..bytes.parse().unwrap_or(0_usize))
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    (state >> 24) as u8
                })
                .collect();
            let _ = out.write_all(&noise);
        }
        _ => {
            eprintln!("unknown command");
            std::process::exit(64);
        }
    }
}

/// Text that repeats with variation, so a compressor finds a lot and not everything.
fn varied(length: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(length + 80);
    let mut n = 0_u32;
    while out.len() < length {
        out.extend_from_slice(
            format!(
                "line {n}: the quick brown fox {} jumps over the lazy dog {}\n",
                n % 17,
                n.wrapping_mul(2_654_435_761) % 1000
            )
            .as_bytes(),
        );
        n += 1;
    }
    out.truncate(length);
    out
}
