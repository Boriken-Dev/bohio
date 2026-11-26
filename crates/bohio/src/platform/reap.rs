//! Collecting the guardians a host adopts when it is process 1 (Linux).
//!
//! A guardian (`guard.rs`) is a grandchild of the host, so when the program's group is ended and the guardian with
//! it, the process that adopted it must collect it, or it stays a zombie of the host for good. Only process 1 (a
//! container's entrypoint) adopts it; elsewhere the init system does. The sweep collects processes named
//! [`super::guard::NAME`] whose parent is this process, and nothing else: a program the host started and waits for
//! itself is never touched.

#![forbid(unsafe_code)]

use std::sync::OnceLock;
use std::time::Duration;

use rustix::process::{Pid, WaitOptions, waitpid};

/// The ended guardians whose parent is `parent`.
pub(super) fn ended_guardians(parent: u32) -> Vec<i32> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let number = entry.file_name().to_str()?.parse::<i32>().ok()?;
            let stat = std::fs::read_to_string(entry.path().join("stat")).ok()?;
            is_ended_guardian_of(&stat, parent).then_some(number)
        })
        .collect()
}

/// Collects every ended guardian whose parent is `parent`; returns how many.
pub(super) fn sweep(parent: u32) -> usize {
    ended_guardians(parent)
        .into_iter()
        .filter_map(Pid::from_raw)
        .filter(|pid| matches!(waitpid(Some(*pid), WaitOptions::NOHANG), Ok(Some(_))))
        .count()
}

/// Whether a `/proc/<pid>/stat` line says a zombie named like a guardian whose parent is `parent`. The command name
/// sits in parentheses and may hold spaces and parentheses, so the fields after it are read from the last one.
fn is_ended_guardian_of(stat: &str, parent: u32) -> bool {
    let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
        return false;
    };
    if &stat[open + 1..close] != super::guard::NAME {
        return false;
    }
    let mut fields = stat[close + 1..].split_whitespace();
    let state = fields.next();
    let ppid = fields.next().and_then(|field| field.parse::<u32>().ok());
    state == Some("Z") && ppid == Some(parent)
}

/// Starts the thread that sweeps every `interval`, once for the process.
pub(super) fn start(interval: Duration) {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let parent = std::process::id();
    let spawned = std::thread::Builder::new()
        .name("bohio-reap".to_owned())
        .spawn(move || {
            loop {
                std::thread::sleep(interval);
                sweep(parent);
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "the thread that collects ended guardians did not start");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_ended_guardian_of_the_parent_is_taken() {
        let line =
            |name: &str, state: &str, ppid: u32| format!("77 ({name}) {state} {ppid} 77 77 0 -1");
        let guard = super::super::guard::NAME;
        assert!(is_ended_guardian_of(&line(guard, "Z", 1), 1));
        assert!(!is_ended_guardian_of(&line(guard, "S", 1), 1));
        assert!(!is_ended_guardian_of(&line(guard, "Z", 2), 1));
        assert!(!is_ended_guardian_of(&line("sh", "Z", 1), 1));
        assert!(!is_ended_guardian_of(&line("a) Z 1 (b", "Z", 1), 1));
    }
}
