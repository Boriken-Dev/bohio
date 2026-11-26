//! A process a program leaves running holds the output pipe, and the host thread that reads it is let go with the program.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

mod support;

use std::time::{Duration, Instant};

use bohio::config::ExecutionConfig;
use bohio::{Grant, Permission};
use bohio_client::exec::{Event, Exec};
use bohio_proto::exec::Request;
use support::{Link, WAIT};

/// How many threads of this process are named `bohio-exec-read`.
fn readers() -> usize {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .flatten()
        .filter(|task| {
            std::fs::read_to_string(task.path().join("comm"))
                .is_ok_and(|name| name.trim() == "bohio-exec-read")
        })
        .count()
}

#[test]
fn a_survivor_holding_the_output_pipe_costs_no_host_thread_once_the_program_is_let_go() {
    if !std::path::Path::new("/usr/bin/setsid").exists()
        && !std::path::Path::new("/bin/setsid").exists()
    {
        eprintln!("SKIPPED: no `setsid` to leave a process outside the program's tree");
        return;
    }
    let link = Link::new(
        Grant::allowing([Permission::Exec]),
        ExecutionConfig::default(),
    );
    // The survivor leaves the program's group and session and keeps its standard output; it ends by itself.
    let request = Request::argv(["sh", "-c", "setsid sleep 12 & echo started"]);
    let exec = Exec::start(&link, &request).unwrap();
    loop {
        match exec.next(WAIT).unwrap() {
            Some(Event::Exit { .. }) | None => break,
            Some(_) => {}
        }
    }
    drop(exec);
    let until = Instant::now() + Duration::from_secs(5);
    while readers() > 0 {
        assert!(
            Instant::now() < until,
            "{} reader threads still wait on a pipe a survivor holds",
            readers()
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
