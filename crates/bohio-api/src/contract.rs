// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The contract suite: what every provider of a kind passes, written once beside the kind.
//!
//! Each function takes a source (an `Instance<dyn K>` or any implementation, as `&dyn K`) and panics with a message
//! naming what the source got wrong. They check what `describe` says against what the source does, that a call
//! comes back in the time it was given, and that nothing panics on an argument that is wrong but possible. A
//! provider's own tests call them over an endpoint the test starts.

mod exec;

use std::time::{Duration, Instant};

use guatiao::Status;
use guatiao::library::ProviderError;

pub use self::exec::exec;
use crate::desktop::DesktopSource;
use crate::media::MediaSource;
use crate::model::{InputModel, ProgramSpec, SourceInfo, TerminalOpen};
use crate::power::PowerSource;
use crate::terminal::TerminalSource;
use crate::vocab;

/// What the suite waits, takes and bounds, as parameters rather than constants.
#[derive(Debug, Clone, Copy)]
pub struct ContractSettings {
    /// How long past the time it was given a call may take before the suite calls it late.
    pub slack: Duration,
    /// The wait a `read` is given.
    pub read_wait: Duration,
    /// How many slacks an exec program may run before the suite calls it endless.
    pub exec_slacks: u32,
    /// The most listing reads the suite makes of one directory before it calls the listing endless.
    pub listing_reads: usize,
    /// The maximum the suite asks of each listing read.
    pub listing_page: u64,
    /// What the suite types at an available terminal: real bytes, which the terminal takes whole.
    pub terminal_input: &'static [u8],
    /// Output the terminal must produce (as the answer to `terminal_input` or by itself) within
    /// `terminal_output_wait`; `None` checks that a read answers, not what it answers.
    pub terminal_output: Option<&'static [u8]>,
    /// How long the suite waits for `terminal_output`.
    pub terminal_output_wait: Duration,
    /// The power actions the suite may take of a source that offers them. Taking one acts on the system, so the
    /// default takes none and a real host is never powered off; a source over a test system names them all.
    pub power_actions: &'static [&'static str],
}

impl Default for ContractSettings {
    fn default() -> Self {
        Self {
            slack: Duration::from_secs(2),
            read_wait: Duration::from_millis(100),
            exec_slacks: 5,
            listing_reads: 100_000,
            listing_page: 16,
            terminal_input: b"echo bohio\n",
            terminal_output: None,
            terminal_output_wait: Duration::from_secs(10),
            power_actions: &[],
        }
    }
}

fn offered(info: &SourceInfo, word: &str) -> bool {
    info.offers.iter().any(|offered| offered == word)
}

fn status_of(error: &ProviderError) -> Status {
    error.status
}

fn describe_of(
    what: &str,
    first: Result<SourceInfo, ProviderError>,
    again: Result<SourceInfo, ProviderError>,
) -> SourceInfo {
    let info = first.unwrap_or_else(|error| panic!("{what}: describe failed: {error:?}"));
    assert!(
        !info.provider.is_empty(),
        "{what}: describe names no provider"
    );
    assert_eq!(
        info.available,
        info.reason.is_none(),
        "{what}: an unavailable source says why and an available one does not (available {}, reason {:?})",
        info.available,
        info.reason
    );
    let again = again.unwrap_or_else(|error| panic!("{what}: describe twice failed: {error:?}"));
    assert_eq!(
        info.provider, again.provider,
        "{what}: describe changes its provider"
    );
    for word in &info.offers {
        assert!(
            !info.lacks.contains(word),
            "{what}: describe both offers and lacks `{word}`"
        );
    }
    info
}

/// The suite of the terminal kind. `open` is what the suite opens the terminal with (a console source refuses a
/// program, so the caller says which request is meaningful). A source that is not available is checked for
/// saying so and for refusing to open, nothing more.
///
/// # Panics
///
/// With a message naming what the source got wrong.
pub fn terminal(source: &dyn TerminalSource, open: TerminalOpen, settings: &ContractSettings) {
    let info = describe_of("terminal", source.describe(), source.describe());
    if !info.available {
        assert!(
            source.open(open).is_err(),
            "terminal: an unavailable source opened a terminal"
        );
        return;
    }
    let terminal = source
        .open(open)
        .unwrap_or_else(|error| panic!("terminal: open failed: {error:?}"));

    // A read returns within the wait plus the slack, with a count, a timeout (0) or the end (-1).
    let mut buffer = [0_u8; 256];
    let started = Instant::now();
    let read = terminal
        .read(
            &mut buffer,
            u64::try_from(settings.read_wait.as_millis()).unwrap_or(u64::MAX),
        )
        .unwrap_or_else(|error| panic!("terminal: read failed: {error:?}"));
    assert!(
        started.elapsed() <= settings.read_wait + settings.slack,
        "terminal: a read of {:?} took {:?}",
        settings.read_wait,
        started.elapsed()
    );
    assert!(read >= -1, "terminal: read answered {read}");
    assert!(
        usize::try_from(read).map_or(true, |count| count <= buffer.len()),
        "terminal: read answered more than the buffer holds"
    );
    let mut seen = buffer[..usize::try_from(read).unwrap_or(0)].to_vec();
    // An empty buffer is not a reason to panic.
    let _ = terminal.read(&mut [], 0);

    // Writing nothing is allowed; real bytes are taken whole, or refused as `unsupported` when `write` is lacked.
    assert_eq!(
        terminal.write(&[]).unwrap_or(0),
        0,
        "terminal: an empty write took bytes"
    );
    let typed = terminal.write(settings.terminal_input);
    if info.lacks(vocab::terminal::WRITE) {
        assert_eq!(
            typed.as_ref().err().map(status_of),
            Some(Status::GUATIAO_ERR_UNSUPPORTED),
            "terminal: lacks `write` and does not answer unsupported: {typed:?}"
        );
    } else {
        let taken = typed.unwrap_or_else(|error| panic!("terminal: write failed: {error:?}"));
        assert_eq!(
            taken,
            settings.terminal_input.len() as u64,
            "terminal: a write of {} bytes took {taken}",
            settings.terminal_input.len()
        );
    }

    // The output the terminal is to print arrives within the wait.
    if let Some(want) = settings.terminal_output {
        let end = Instant::now() + settings.terminal_output_wait;
        let wait = u64::try_from(settings.read_wait.as_millis()).unwrap_or(u64::MAX);
        while !seen.windows(want.len().max(1)).any(|window| window == want) && Instant::now() < end
        {
            let count = terminal
                .read(&mut buffer, wait)
                .unwrap_or_else(|error| panic!("terminal: read failed: {error:?}"));
            if count < 0 {
                break;
            }
            seen.extend_from_slice(&buffer[..usize::try_from(count).unwrap_or(0)]);
        }
        assert!(
            seen.windows(want.len().max(1)).any(|window| window == want),
            "terminal: the output {:?} never arrived, read {:?}",
            String::from_utf8_lossy(want),
            String::from_utf8_lossy(&seen)
        );
    }

    // What `describe` says it lacks, the terminal refuses as `unsupported`; what it offers, it does.
    let resize = terminal.resize(80, 24);
    let signal = terminal.signal("interrupt");
    if info.lacks(vocab::terminal::RESIZE) {
        assert_eq!(
            resize.as_ref().err().map(status_of),
            Some(Status::GUATIAO_ERR_UNSUPPORTED),
            "terminal: lacks `resize` and does not answer unsupported: {resize:?}"
        );
    } else if offered(&info, vocab::terminal::RESIZE) {
        assert!(
            resize.is_ok(),
            "terminal: offers `resize` and fails it: {resize:?}"
        );
    }
    if info.lacks(vocab::terminal::SIGNALS) {
        assert_eq!(
            signal.as_ref().err().map(status_of),
            Some(Status::GUATIAO_ERR_UNSUPPORTED),
            "terminal: lacks `signals` and does not answer unsupported: {signal:?}"
        );
    } else if offered(&info, vocab::terminal::SIGNALS) {
        let delivery = signal
            .unwrap_or_else(|error| panic!("terminal: offers `signals` and fails it: {error:?}"));
        assert!(
            delivery.delivered,
            "terminal: offers `signals` and did not deliver `interrupt`: {delivery:?}"
        );
    }
    // A size no terminal has, and a signal no one knows, are refused or ignored, never a panic.
    let _ = terminal.resize(0, 0);
    let _ = terminal.signal("no-such-signal");

    let exit = terminal.exit();
    assert!(exit.is_ok(), "terminal: exit failed: {exit:?}");
    if info.lacks(vocab::terminal::EXIT_STATUS)
        && let Ok(Some(exit)) = exit
    {
        assert!(
            exit.code.is_none(),
            "terminal: lacks `exit_status` and reports a code"
        );
    }
    let _ = terminal.kill();

    // A source that starts no program refuses a request that names one.
    if info.lacks(vocab::terminal::PROGRAMS) {
        let request = TerminalOpen {
            program: Some(ProgramSpec {
                argv: vec!["bohio-contract-no-such-program".to_owned()],
                ..ProgramSpec::default()
            }),
            ..TerminalOpen::default()
        };
        let answer = source.open(request).map(drop);
        assert!(
            matches!(
                answer.as_ref().err().map(status_of),
                Some(Status::GUATIAO_ERR_BAD_VALUE | Status::GUATIAO_ERR_UNSUPPORTED)
            ),
            "terminal: lacks `programs` and opened one: {answer:?}"
        );
    }
}

/// The suite of the desktop kind.
///
/// # Panics
///
/// With a message naming what the source got wrong.
pub fn desktop(source: &dyn DesktopSource, settings: &ContractSettings) {
    let info = describe_of("desktop", source.describe(), source.describe());
    if !info.available {
        assert!(
            source.open().is_err(),
            "desktop: an unavailable source opened a session"
        );
        return;
    }
    let session = source
        .open()
        .unwrap_or_else(|error| panic!("desktop: open failed: {error:?}"));
    let started = Instant::now();
    let state = session
        .state()
        .unwrap_or_else(|error| panic!("desktop: state failed: {error:?}"));
    assert!(
        started.elapsed() <= settings.slack,
        "desktop: state took {:?}",
        started.elapsed()
    );
    let display = state
        .displays
        .iter()
        .find(|display| display.primary)
        .or_else(|| state.displays.first());
    if let Some(display) = display
        && offered(&info, vocab::desktop::CAPTURE)
    {
        let header = session
            .frame_size(&display.id, None)
            .unwrap_or_else(|error| panic!("desktop: frame_size failed: {error:?}"));
        assert!(
            !header.is_empty(),
            "desktop: frame_size of a display is empty"
        );
        let mut frame = vec![0_u8; usize::try_from(header.len()).expect("a frame fits memory")];
        let written = session
            .capture(&display.id, None, &mut frame)
            .unwrap_or_else(|error| panic!("desktop: capture failed: {error:?}"));
        assert_eq!(
            written, header,
            "desktop: capture wrote a frame other than frame_size said"
        );
    }
    // A display no desktop has is `not_found`, never a panic.
    let missing = session.frame_size("no-such-display", None);
    assert_eq!(
        missing.as_ref().err().map(status_of),
        Some(Status::GUATIAO_ERR_NOT_FOUND),
        "desktop: frame_size of a display it does not have is not not_found: {missing:?}"
    );
    let mut small = [0_u8; 3];
    let missing = session.capture("no-such-display", None, &mut small);
    assert_eq!(
        missing.as_ref().err().map(status_of),
        Some(Status::GUATIAO_ERR_NOT_FOUND),
        "desktop: capture of a display it does not have is not not_found: {missing:?}"
    );
    let pointer_move = || InputModel {
        kind: vocab::input::MOVE.to_owned(),
        x: Some(0),
        y: Some(0),
        ..InputModel::default()
    };
    if info.lacks(vocab::desktop::INPUT) {
        let answer = session.input(vec![pointer_move()]);
        assert_eq!(
            answer.as_ref().err().map(status_of),
            Some(Status::GUATIAO_ERR_UNSUPPORTED),
            "desktop: lacks `input` and does not answer unsupported: {answer:?}"
        );
    } else if offered(&info, vocab::desktop::INPUT) {
        let taken = session
            .input(vec![pointer_move()])
            .unwrap_or_else(|error| panic!("desktop: offers `input` and fails it: {error:?}"));
        assert_eq!(
            taken, 1,
            "desktop: one pointer move was taken as {taken} events"
        );
    }
    if info.lacks(vocab::desktop::CLIPBOARD) {
        let answer = session.clipboard_text();
        assert_eq!(
            answer.as_ref().err().map(status_of),
            Some(Status::GUATIAO_ERR_UNSUPPORTED),
            "desktop: lacks `clipboard` and does not answer unsupported: {answer:?}"
        );
        let answer = session.set_clipboard_text("bohio contract");
        assert_eq!(
            answer.as_ref().err().map(status_of),
            Some(Status::GUATIAO_ERR_UNSUPPORTED),
            "desktop: lacks `clipboard` and sets it: {answer:?}"
        );
    } else if offered(&info, vocab::desktop::CLIPBOARD) {
        // What the clipboard held is put back, so a suite over a person's desktop leaves it as it was.
        let before = session.clipboard_text().ok();
        let text = "bohio contract clipboard";
        session.set_clipboard_text(text).unwrap_or_else(|error| {
            panic!("desktop: offers `clipboard` and fails to set it: {error:?}")
        });
        let read = session.clipboard_text().unwrap_or_else(|error| {
            panic!("desktop: offers `clipboard` and fails to read it: {error:?}")
        });
        if let Some(before) = before {
            let _ = session.set_clipboard_text(&before);
        }
        assert_eq!(read, text, "desktop: the clipboard text did not round-trip");
    }
    // An event kind no source knows is refused or ignored, never a panic.
    let _ = session.input(vec![InputModel {
        kind: "no-such-kind".to_owned(),
        ..InputModel::default()
    }]);
}

/// The suite of the power kind.
///
/// # Panics
///
/// With a message naming what the source got wrong.
pub fn power(source: &dyn PowerSource, settings: &ContractSettings) {
    let info = describe_of("power", source.describe(), source.describe());
    if !info.available {
        assert!(
            source.state().is_err(),
            "power: an unavailable source answered a state"
        );
        return;
    }
    let started = Instant::now();
    let state = source
        .state()
        .unwrap_or_else(|error| panic!("power: state failed: {error:?}"));
    assert!(
        started.elapsed() <= settings.slack,
        "power: state took {:?}",
        started.elapsed()
    );
    assert!(
        [
            vocab::power_state::ON,
            vocab::power_state::OFF,
            vocab::power_state::UNKNOWN
        ]
        .contains(&state.state.as_str()),
        "power: state is `{}`, not one of on, off, unknown",
        state.state
    );
    for action in vocab::power::ACTIONS {
        if info.lacks(action) {
            let answer = source.act(action);
            assert_eq!(
                answer.as_ref().err().map(status_of),
                Some(Status::GUATIAO_ERR_UNSUPPORTED),
                "power: lacks `{action}` and does not answer unsupported: {answer:?}"
            );
        } else if offered(&info, action) && settings.power_actions.contains(&action) {
            let answer = source
                .act(action)
                .unwrap_or_else(|error| panic!("power: offers `{action}` and fails it: {error:?}"));
            assert!(
                [
                    vocab::power_state::ON,
                    vocab::power_state::OFF,
                    vocab::power_state::UNKNOWN
                ]
                .contains(&answer.state.as_str()),
                "power: `{action}` answered the state `{}`, not one of on, off, unknown",
                answer.state
            );
        }
    }
    // A word no source knows is refused, never taken for an action.
    assert!(
        source.act("no-such-action").is_err(),
        "power: an action nobody defined succeeded"
    );
}

/// The suite of the media kind.
///
/// # Panics
///
/// With a message naming what the source got wrong.
pub fn media(source: &dyn MediaSource, settings: &ContractSettings) {
    let info = describe_of("media", source.describe(), source.describe());
    if !info.available {
        assert!(
            source.status().is_err(),
            "media: an unavailable source answered a status"
        );
        return;
    }
    let started = Instant::now();
    let slots = source
        .status()
        .unwrap_or_else(|error| panic!("media: status failed: {error:?}"));
    assert!(
        started.elapsed() <= settings.slack,
        "media: status took {:?}",
        started.elapsed()
    );
    let inserted = source.insert_url("no-such-slot", "http://example.invalid/image.iso");
    if info.lacks(vocab::media::INSERT_URL) {
        assert_eq!(
            inserted.as_ref().err().map(status_of),
            Some(Status::GUATIAO_ERR_UNSUPPORTED),
            "media: lacks `insert_url` and does not answer unsupported: {inserted:?}"
        );
    } else if offered(&info, vocab::media::INSERT_URL) {
        assert_eq!(
            inserted.as_ref().err().map(status_of),
            Some(Status::GUATIAO_ERR_NOT_FOUND),
            "media: offers `insert_url` and took a slot it does not have: {inserted:?}"
        );
    }
    let mut seen = Vec::new();
    for slot in &slots {
        assert!(
            !slot.slot.is_empty(),
            "media: status names a slot with no name"
        );
        assert!(
            !seen.contains(&slot.slot),
            "media: status names the slot `{}` twice",
            slot.slot
        );
        seen.push(slot.slot.clone());
        if !slot.inserted {
            assert!(
                slot.image.is_none() && slot.size.is_none(),
                "media: the empty slot `{}` names an image or a size: {slot:?}",
                slot.slot
            );
        }
    }
    let eject = source.eject("no-such-slot");
    if info.lacks(vocab::media::EJECT) {
        assert_eq!(
            eject.as_ref().err().map(status_of),
            Some(Status::GUATIAO_ERR_UNSUPPORTED),
            "media: lacks `eject` and does not answer unsupported: {eject:?}"
        );
    } else {
        assert_eq!(
            eject.as_ref().err().map(status_of),
            Some(Status::GUATIAO_ERR_NOT_FOUND),
            "media: ejected a slot it does not have: {eject:?}"
        );
        // Ejecting what is empty changes nothing and answers the slot as it is.
        if let Some(empty) = slots.iter().find(|slot| !slot.inserted) {
            let ejected = source
                .eject(&empty.slot)
                .unwrap_or_else(|error| panic!("media: ejecting an empty slot failed: {error:?}"));
            assert!(
                ejected.slot == empty.slot && !ejected.inserted,
                "media: ejecting the empty slot `{}` answered {ejected:?}",
                empty.slot
            );
        }
    }
}

mod files;
pub use files::files;
