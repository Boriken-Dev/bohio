// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The words a source and a host agree on.
//!
//! A word a host does not know is carried and shown as it is, never refused: the vocabularies grow.

/// What a terminal source offers (`SourceInfo::offers`) and lacks (`SourceInfo::lacks`).
pub mod terminal {
    /// Output can be read.
    pub const READ: &str = "read";
    /// Input can be written.
    pub const WRITE: &str = "write";
    /// The source starts programs (a pseudo-terminal does; a serial console does not).
    pub const PROGRAMS: &str = "programs";
    /// The terminal's size can change.
    pub const RESIZE: &str = "resize";
    /// Signals reach the program.
    pub const SIGNALS: &str = "signals";
    /// The program's end carries an exit status.
    pub const EXIT_STATUS: &str = "exit_status";
    /// A BREAK condition can be sent.
    pub const BREAK: &str = "break";
}

/// What a desktop source offers and lacks.
pub mod desktop {
    /// A picture of a display.
    pub const CAPTURE: &str = "capture";
    /// Keyboard and mouse input.
    pub const INPUT: &str = "input";
    /// The clipboard's text.
    pub const CLIPBOARD: &str = "clipboard";
    /// The screen's lock can be told.
    pub const LOCK_DETECTION: &str = "lock_detection";
    /// More than one display.
    pub const MULTI_DISPLAY: &str = "multi_display";
}

/// What a power source offers: the actions it takes.
pub mod power {
    /// Read the state.
    pub const STATUS: &str = "status";
    /// Power on.
    pub const ON: &str = "on";
    /// Power off at once.
    pub const OFF: &str = "off";
    /// Ask the system to shut down.
    pub const SOFT_OFF: &str = "soft_off";
    /// Power off and on.
    pub const CYCLE: &str = "cycle";
    /// Reset without cutting power.
    pub const RESET: &str = "reset";
    /// A non-maskable interrupt.
    pub const NMI: &str = "nmi";
    /// Every action, in the order a command lists them.
    pub const ACTIONS: [&str; 6] = [ON, OFF, SOFT_OFF, CYCLE, RESET, NMI];
}

/// What a media source offers.
pub mod media {
    /// Read the slots.
    pub const STATUS: &str = "status";
    /// Insert an image named by a URL the source fetches.
    pub const INSERT_URL: &str = "insert_url";
    /// Insert an image the host serves block by block.
    pub const INSERT_READER: &str = "insert_reader";
    /// Eject.
    pub const EJECT: &str = "eject";
}

/// The `kind` of an input event.
pub mod input {
    /// Moves the pointer.
    pub const MOVE: &str = "move";
    /// A button.
    pub const BUTTON: &str = "button";
    /// The wheel.
    pub const SCROLL: &str = "scroll";
    /// A key.
    pub const KEY: &str = "key";
    /// Text typed as it is.
    pub const TEXT: &str = "text";
}

/// A power state's name.
pub mod power_state {
    /// Powered.
    pub const ON: &str = "on";
    /// Not powered.
    pub const OFF: &str = "off";
    /// The source cannot tell.
    pub const UNKNOWN: &str = "unknown";
}
