//! A started program's signal dispositions are the defaults, whatever the host inherited.
//!
//! The outer test starts this test binary under a shell that ignores SIGINT and SIGQUIT (as `nohup` and a
//! background job of a non-interactive shell do); the inner one starts a program through [`spawn`] and reads which
//! signals that program ignores.

#![forbid(unsafe_code)]

use std::ffi::OsString;
use std::io::Read;
use std::process::Command;

use bohio_proto::exec::Env;

use super::{Spec, spawn};

const PROBE: &str = "BOHIO_SIGNAL_PROBE";

fn spec(reset_signals: bool) -> Spec {
    Spec {
        program: OsString::from("grep"),
        args: vec![
            OsString::from("SigIgn"),
            OsString::from("/proc/self/status"),
        ],
        raw_args: None,
        cwd: None,
        env: Env::default(),
        stdin: false,
        on_host_death: crate::knobs::HostDeath::default(),
        reset_signals,
    }
}

/// The ignored-signal mask of a program started with `reset_signals`.
fn ignored(reset_signals: bool) -> u64 {
    let mut started = spawn(spec(reset_signals)).expect("grep starts");
    let mut text = String::new();
    started
        .stdout
        .read_to_string(&mut text)
        .expect("its output");
    let _ = started.control.kill();
    let mask = text.split_whitespace().nth(1).expect("a mask");
    u64::from_str_radix(mask, 16).expect("a hex mask")
}

#[test]
fn a_probe_reports_the_ignored_signals_of_the_program_it_starts() {
    if std::env::var_os(PROBE).is_none() {
        return;
    }
    // SIGINT is bit 1 and SIGQUIT bit 2 of the mask.
    assert_eq!(
        ignored(true) & 0b110,
        0,
        "the program inherited ignored signals"
    );
    assert_eq!(
        ignored(false) & 0b110,
        0b110,
        "without the reset the program keeps what the host had"
    );
}

#[test]
fn a_host_that_ignores_interrupt_and_quit_starts_programs_that_do_not() {
    let me = std::env::current_exe().expect("this binary");
    let status = Command::new("sh")
        .args([
            "-c",
            "trap '' INT QUIT; exec \"$0\" --exact platform::signal_tests::a_probe_reports_the_ignored_signals_of_the_program_it_starts --test-threads=1",
        ])
        .arg(me)
        .env(PROBE, "1")
        .status()
        .expect("sh starts");
    assert!(status.success(), "the probe failed: {status}");
}
