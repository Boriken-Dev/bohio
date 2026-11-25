// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The suite of the exec kind.

use std::time::{Duration, Instant};

use guatiao::Status;
use guatiao::library::ProviderError;

use super::{ContractSettings, describe_of, status_of};
use crate::exec::{ExecSource, Process, STDERR, STDOUT};
use crate::model::{ExecStart, TerminalExit};
use crate::vocab;

/// The suite of the exec kind. `start` is a program that prints something and ends by itself within a few
/// seconds (the caller says which one is meaningful on the system the source runs on). A source that is not
/// available is checked for saying so and for refusing to start, nothing more.
///
/// # Panics
///
/// With a message naming what the source got wrong.
pub fn exec(source: &dyn ExecSource, start: &ExecStart, settings: &ContractSettings) {
    let info = describe_of("exec", source.describe(), source.describe());
    if !info.available {
        assert!(
            source.start(start.clone()).is_err(),
            "exec: an unavailable source started a process"
        );
        return;
    }
    let deadline = settings.slack * 5;
    let process = source
        .start(start.clone())
        .unwrap_or_else(|error| panic!("exec: start failed: {error:?}"));

    let (output, exit) = run_to_the_end(&*process, settings, deadline);
    assert!(
        !output.is_empty(),
        "exec: the program printed something and no byte arrived"
    );
    let exit = exit.unwrap_or_else(|| panic!("exec: the program did not end within {deadline:?}"));
    assert!(
        exit.code.is_some() || exit.signal.is_some(),
        "exec: a program that ended carries neither a code nor a signal"
    );

    // Ending a process that has ended answers the same, every time.
    let first = process.kill().unwrap_or_else(|error| {
        panic!("exec: kill of an ended process failed: {error:?}");
    });
    let second = process
        .kill()
        .unwrap_or_else(|error| panic!("exec: a second kill failed: {error:?}"));
    assert_eq!(
        first, second,
        "exec: kill answers differently the second time"
    );
    assert_eq!(
        process.wait(0).ok().flatten(),
        Some(first),
        "exec: wait after kill disagrees with kill"
    );

    // What is wrong but possible does not panic: an empty buffer, an empty write, a signal of no name.
    let _ = process.read(STDOUT, &mut [], 0);
    let _ = process.write(&[]);
    let _ = process.close_input();
    let _ = process.signal("no-such-signal");

    // What `describe` lacks, the process refuses as `unsupported`.
    if info.lacks(vocab::exec::SIGNALS) {
        let signal = process.signal("interrupt");
        assert_eq!(
            signal.as_ref().err().map(status_of),
            Some(Status::GUATIAO_ERR_UNSUPPORTED),
            "exec: lacks `signals` and does not answer unsupported: {signal:?}"
        );
    }

    // A program that does not exist is `NOT_FOUND` at start, or a process that ends with a failure (a source that
    // starts a remote shell cannot tell before it runs).
    let missing = ExecStart {
        argv: vec!["bohio-contract-no-such-program".to_owned()],
        shell: None,
        script: None,
        ..start.clone()
    };
    match source.start(missing) {
        Err(error) => assert_eq!(
            status_of(&error),
            Status::GUATIAO_ERR_NOT_FOUND,
            "exec: a missing program is not NOT_FOUND: {error:?}"
        ),
        Ok(process) => {
            let (_, exit) = run_to_the_end(&*process, settings, deadline);
            let exit = exit.expect("exec: a missing program neither failed to start nor ended");
            assert!(
                exit.code != Some(0),
                "exec: a program that does not exist ended with code 0"
            );
        }
    }

    // An empty request names nothing to run.
    let empty = ExecStart {
        argv: Vec::new(),
        shell: None,
        script: None,
        ..start.clone()
    };
    if let Ok(process) = source.start(empty) {
        let _ = process.kill();
    }
}

/// Reads both outputs until each has ended and waits for the exit, within `deadline`: what arrived, and how the
/// program ended when it did.
fn run_to_the_end(
    process: &dyn Process,
    settings: &ContractSettings,
    deadline: Duration,
) -> (Vec<u8>, Option<TerminalExit>) {
    let wait = u64::try_from(settings.read_wait.as_millis()).unwrap_or(u64::MAX);
    let started = Instant::now();
    let mut ended = [false, false];
    let mut output = Vec::new();
    let mut buffer = [0_u8; 256];
    while !(ended[0] && ended[1]) && started.elapsed() < deadline {
        for (at, stream) in [STDOUT, STDERR].into_iter().enumerate() {
            if ended[at] {
                continue;
            }
            let began = Instant::now();
            let read = process
                .read(stream, &mut buffer, wait)
                .unwrap_or_else(|error| panic!("exec: read failed: {error:?}"));
            assert!(
                began.elapsed() <= settings.read_wait + settings.slack,
                "exec: a read of {:?} took {:?}",
                settings.read_wait,
                began.elapsed()
            );
            assert!(read >= -1, "exec: read answered {read}");
            match usize::try_from(read) {
                Ok(count) => {
                    assert!(
                        count <= buffer.len(),
                        "exec: read answered more than the buffer holds"
                    );
                    output.extend_from_slice(&buffer[..count]);
                }
                Err(_) => ended[at] = true,
            }
        }
    }
    let left = deadline.saturating_sub(started.elapsed());
    let exit = process
        .wait(u64::try_from(left.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or_else(|error: ProviderError| panic!("exec: wait failed: {error:?}"));
    (output, exit)
}
