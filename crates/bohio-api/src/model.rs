// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The models that cross the kinds.
//!
//! A model grows by fields that default, never by a required field. Vocabularies (what a source `offers` and
//! `lacks`, a power action, a signal, an input kind) are text, spelled as [`crate::vocab`] names them, so a
//! provider in another language sends the same strings and a host meets a word it does not know without failing.

use guatiao::{FromValue, Schema, ToValue};

/// What a source says about itself, asked at every `info` and never cached.
#[derive(ToValue, FromValue, Schema, Debug, Clone, Default, PartialEq, Eq)]
#[map(crate = ::guatiao)]
pub struct SourceInfo {
    /// The provider's id.
    pub provider: String,
    /// What this source can do, in its kind's vocabulary ([`crate::vocab`]).
    #[map(default)]
    pub offers: Vec<String>,
    /// What this source cannot do, in the kind's vocabulary: the limits a peer should know before it asks.
    #[map(default)]
    pub lacks: Vec<String>,
    /// Whether the source can be used now.
    #[map(default)]
    pub available: bool,
    /// Why it cannot, when `available` is false.
    pub reason: Option<String>,
}

impl SourceInfo {
    /// An available source of `provider` offering `offers` and lacking `lacks`.
    #[must_use]
    pub fn new(provider: &str, offers: &[&str], lacks: &[&str]) -> Self {
        Self {
            provider: provider.to_owned(),
            offers: offers.iter().map(|word| (*word).to_owned()).collect(),
            lacks: lacks.iter().map(|word| (*word).to_owned()).collect(),
            available: true,
            reason: None,
        }
    }

    /// The same source, unavailable for `reason`.
    #[must_use]
    pub fn unavailable(mut self, reason: &str) -> Self {
        self.available = false;
        self.reason = Some(reason.to_owned());
        self
    }

    /// Whether the source says it lacks `word`.
    #[must_use]
    pub fn lacks(&self, word: &str) -> bool {
        self.lacks.iter().any(|lacking| lacking == word)
    }
}

/// One environment change of a program's start.
#[derive(ToValue, FromValue, Schema, Debug, Clone, PartialEq, Eq)]
#[map(crate = ::guatiao)]
pub struct EnvVar {
    /// The variable.
    pub name: String,
    /// Its new value; absent removes it.
    pub value: Option<String>,
}

/// A program a terminal starts, already decided by the host: the grant's checks are made before a source sees it.
#[derive(ToValue, FromValue, Schema, Debug, Clone, Default, PartialEq, Eq)]
#[map(crate = ::guatiao)]
pub struct ProgramSpec {
    /// A shell kind (`"sh"`, `"bash"`, `"powershell"`, `"cmd"`...) the source finds itself; empty when `argv` names the program.
    pub kind: Option<String>,
    /// The program and its arguments.
    #[map(default)]
    pub argv: Vec<String>,
    /// The grant lists programs: the program is the one found at that name on `path`, never what the request's
    /// environment would find.
    #[map(default)]
    pub listed: bool,
}

/// What a terminal source is asked to open.
#[derive(ToValue, FromValue, Schema, Debug, Clone, PartialEq, Eq)]
#[map(crate = ::guatiao)]
pub struct TerminalOpen {
    /// The program to start. A console source starts none and refuses a request that names one.
    pub program: Option<ProgramSpec>,
    /// The working directory.
    pub cwd: Option<String>,
    /// Start from an empty environment.
    #[map(default)]
    pub clear_env: bool,
    /// Changes to the environment, in order.
    #[map(default)]
    pub env: Vec<EnvVar>,
    /// The host's own `PATH`, where a program without a directory is looked for.
    pub path: Option<String>,
    /// Columns.
    pub cols: u16,
    /// Rows.
    pub rows: u16,
}

impl Default for TerminalOpen {
    fn default() -> Self {
        Self {
            program: None,
            cwd: None,
            clear_env: false,
            env: Vec::new(),
            path: None,
            cols: 80,
            rows: 24,
        }
    }
}

/// How a terminal's program ended.
#[derive(ToValue, FromValue, Schema, Debug, Clone, Default, PartialEq, Eq)]
#[map(crate = ::guatiao)]
pub struct TerminalExit {
    /// The exit code, when the source knows one.
    pub code: Option<i64>,
    /// The signal that ended it, by name, when one did.
    pub signal: Option<String>,
}

/// What a signal request came to.
#[derive(ToValue, FromValue, Schema, Debug, Clone, Default, PartialEq, Eq)]
#[map(crate = ::guatiao)]
pub struct Delivery {
    /// The signal reached the program.
    #[map(default)]
    pub delivered: bool,
    /// Delivering it meant ending the program.
    #[map(default)]
    pub ended: bool,
    /// Something a person should know: why it was not delivered, or what was done instead.
    pub message: Option<String>,
}

/// One display of a desktop.
#[derive(ToValue, FromValue, Schema, Debug, Clone, Default, PartialEq)]
#[map(crate = ::guatiao)]
pub struct DisplayModel {
    /// How requests name it.
    pub id: String,
    /// A name for a person.
    pub name: Option<String>,
    /// The display a request without a display means.
    #[map(default)]
    pub primary: bool,
    /// Its left edge in the desktop's layout.
    pub x: i64,
    /// Its top edge.
    pub y: i64,
    /// Width in physical pixels.
    pub width: u64,
    /// Height in physical pixels.
    pub height: u64,
    /// Physical pixels per logical point.
    #[map(default = 1.0)]
    pub scale: f64,
    /// Rotation in degrees, when known.
    pub rotation: Option<u64>,
}

/// What a desktop is doing now.
#[derive(ToValue, FromValue, Schema, Debug, Clone, Default, PartialEq)]
#[map(crate = ::guatiao)]
pub struct StateModel {
    /// The account that owns the session, when known.
    pub user: Option<String>,
    /// Whether the session is the one at the screen.
    #[map(default)]
    pub active: bool,
    /// Whether the screen is locked; absent when the source cannot tell.
    pub locked: Option<bool>,
    /// The displays and their layout.
    #[map(default)]
    pub displays: Vec<DisplayModel>,
}

/// A rectangle in a display's pixels.
#[derive(ToValue, FromValue, Schema, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[map(crate = ::guatiao)]
pub struct RectModel {
    /// Left edge.
    pub x: u64,
    /// Top edge.
    pub y: u64,
    /// Width.
    pub width: u64,
    /// Height.
    pub height: u64,
}

/// The size of a captured frame, which is then `width * height * 3` bytes of RGB, rows from the top, no padding.
#[derive(ToValue, FromValue, Schema, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[map(crate = ::guatiao)]
pub struct FrameHeader {
    /// Width in pixels.
    pub width: u64,
    /// Height in pixels.
    pub height: u64,
}

impl FrameHeader {
    /// The bytes the frame takes.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.width.saturating_mul(self.height).saturating_mul(3)
    }

    /// Whether the frame has no pixel.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One input event, flat so it crosses as a map: `kind` is `move`, `button`, `scroll`, `key` or `text`
/// ([`crate::vocab::input`]), and the fields it does not use are absent.
#[derive(ToValue, FromValue, Schema, Debug, Clone, Default, PartialEq)]
#[map(crate = ::guatiao)]
pub struct InputModel {
    /// The kind of event.
    pub kind: String,
    /// The display coordinates are on; the primary one when absent.
    pub display: Option<String>,
    /// Pixels from the display's left edge.
    pub x: Option<i64>,
    /// Pixels from the display's top edge.
    pub y: Option<i64>,
    /// A button (`left`, `right`, `middle`, `back`, `forward`).
    pub button: Option<String>,
    /// `press`, `release` or `click`.
    pub action: Option<String>,
    /// Clicks of a button.
    pub count: Option<u64>,
    /// Horizontal notches of a scroll.
    #[map(default)]
    pub dx: i64,
    /// Vertical notches of a scroll.
    #[map(default)]
    pub dy: i64,
    /// A key name or one character.
    pub key: Option<String>,
    /// Text typed as it is.
    pub text: Option<String>,
}

/// A power state with whether an action finished.
#[derive(ToValue, FromValue, Schema, Debug, Clone, Default, PartialEq, Eq)]
#[map(crate = ::guatiao)]
pub struct PowerState {
    /// `on`, `off` or `unknown` ([`crate::vocab::power_state`]).
    pub state: String,
    /// The state is the one the action asked for; false while a transition is still under way.
    #[map(default)]
    pub done: bool,
}

/// One virtual-media slot.
#[derive(ToValue, FromValue, Schema, Debug, Clone, Default, PartialEq, Eq)]
#[map(crate = ::guatiao)]
pub struct MediaSlot {
    /// The slot's name (`cd`, `floppy`, `usb`...).
    pub slot: String,
    /// Whether an image is inserted.
    #[map(default)]
    pub inserted: bool,
    /// What is inserted, for a person; never a URL's query.
    pub image: Option<String>,
    /// Size of the inserted image in bytes, when known.
    pub size: Option<u64>,
}
