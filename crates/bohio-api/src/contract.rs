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
use crate::model::{InputModel, SourceInfo, TerminalOpen};
use crate::power::PowerSource;
use crate::terminal::TerminalSource;
use crate::vocab;

/// What the suite waits, as parameters rather than constants.
#[derive(Debug, Clone, Copy)]
pub struct ContractSettings {
    /// How long past the time it was given a call may take before the suite calls it late.
    pub slack: Duration,
    /// The wait a `read` is given.
    pub read_wait: Duration,
}

impl Default for ContractSettings {
    fn default() -> Self {
        Self {
            slack: Duration::from_secs(2),
            read_wait: Duration::from_millis(100),
        }
    }
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
    // An empty buffer is not a reason to panic.
    let _ = terminal.read(&mut [], 0);

    // Writing nothing is allowed; writing a byte takes it or says why.
    assert_eq!(
        terminal.write(&[]).unwrap_or(0),
        0,
        "terminal: an empty write took bytes"
    );

    // What `describe` says it lacks, the terminal refuses as `unsupported`; what it offers, it does.
    let resize = terminal.resize(80, 24);
    let signal = terminal.signal("interrupt");
    if info.lacks(vocab::terminal::RESIZE) {
        assert_eq!(
            resize.as_ref().err().map(status_of),
            Some(Status::GUATIAO_ERR_UNSUPPORTED),
            "terminal: lacks `resize` and does not answer unsupported: {resize:?}"
        );
    } else if info
        .offers
        .iter()
        .any(|word| word == vocab::terminal::RESIZE)
    {
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
        && info
            .offers
            .iter()
            .any(|word| word == vocab::desktop::CAPTURE)
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
    assert!(
        missing.is_err(),
        "desktop: frame_size of a display it does not have succeeded"
    );
    let mut small = [0_u8; 3];
    assert!(
        session
            .capture("no-such-display", None, &mut small)
            .is_err(),
        "desktop: capture of a display it does not have succeeded"
    );
    if info.lacks(vocab::desktop::INPUT) {
        let event = InputModel {
            kind: vocab::input::MOVE.to_owned(),
            x: Some(0),
            y: Some(0),
            ..InputModel::default()
        };
        let answer = session.input(vec![event]);
        assert_eq!(
            answer.as_ref().err().map(status_of),
            Some(Status::GUATIAO_ERR_UNSUPPORTED),
            "desktop: lacks `input` and does not answer unsupported: {answer:?}"
        );
    }
    if info.lacks(vocab::desktop::CLIPBOARD) {
        let answer = session.clipboard_text();
        assert_eq!(
            answer.as_ref().err().map(status_of),
            Some(Status::GUATIAO_ERR_UNSUPPORTED),
            "desktop: lacks `clipboard` and does not answer unsupported: {answer:?}"
        );
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
    if info.lacks(vocab::media::INSERT_URL) {
        let answer = source.insert_url("no-such-slot", "http://example.invalid/image.iso");
        assert_eq!(
            answer.as_ref().err().map(status_of),
            Some(Status::GUATIAO_ERR_UNSUPPORTED),
            "media: lacks `insert_url` and does not answer unsupported: {answer:?}"
        );
    }
    assert!(
        source.eject("no-such-slot").is_err(),
        "media: ejected a slot it does not have"
    );
    let _ = slots;
}

mod files;
pub use files::files;
