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
    /// Programs the source starts get the node's own display: they can read its windows, type into them and set its clipboard.
    pub const X11_FORWARD: &str = "x11_forward";
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

/// What an exec source offers and lacks.
pub mod exec {
    /// Starts a program from an argument vector.
    pub const START: &str = "start";
    /// Standard input.
    pub const STDIN: &str = "stdin";
    /// Standard error apart from standard output.
    pub const STDERR: &str = "stderr";
    /// Signals reach the program.
    pub const SIGNALS: &str = "signals";
    /// A shell kind runs a script.
    pub const SHELL: &str = "shell";
    /// A working directory.
    pub const CWD: &str = "cwd";
    /// Changes to the environment, and starting from an empty one.
    pub const ENV: &str = "env";
    /// A listed program is the one found at that name on the host's `PATH`.
    pub const LISTED: &str = "listed";
    /// Everything the program started ends with it.
    pub const KILL_TREE: &str = "kill_tree";
    /// The program's end carries an exit status.
    pub const EXIT_STATUS: &str = "exit_status";
    /// Programs the source starts get the node's own display: they can read its windows, type into them and set its clipboard.
    pub const X11_FORWARD: &str = "x11_forward";
}

/// What a files source offers and lacks.
pub mod files {
    /// Read a file.
    pub const READ: &str = "read";
    /// Write a file.
    pub const WRITE: &str = "write";
    /// A name cannot leave the directory it is resolved against, whatever links lie on the way.
    pub const CONFINED: &str = "confined";
    /// Symbolic links.
    pub const SYMLINK: &str = "symlink";
    /// Hard links.
    pub const HARD_LINK: &str = "hard_link";
    /// Times can be set.
    pub const SET_TIMES: &str = "set_times";
    /// Permission bits can be set.
    pub const SET_MODE: &str = "set_mode";
    /// The read-only attribute can be set.
    pub const SET_READONLY: &str = "set_readonly";
    /// The space of a file system.
    pub const STATVFS: &str = "statvfs";
    /// A rename replaces its target in one step.
    pub const ATOMIC_REPLACE: &str = "atomic_replace";
    /// A written file can be flushed to the medium.
    pub const SYNC: &str = "sync";
    /// A listing carries each entry's attributes.
    pub const LIST_STATS: &str = "list_stats";
    /// Names are POSIX paths (`/` separators, one root); a source without this word names in the node's own syntax.
    pub const POSIX_PATHS: &str = "posix_paths";
}

/// The `kind` of a [`crate::model::FileStat`].
pub mod file_kind {
    /// A regular file.
    pub const FILE: &str = "file";
    /// A directory.
    pub const DIR: &str = "dir";
    /// A symbolic link.
    pub const SYMLINK: &str = "symlink";
    /// Anything else.
    pub const OTHER: &str = "other";
}
