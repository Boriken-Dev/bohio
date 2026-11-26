// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `bohio-x11`: the desktop a host serves, and the X11 backend that serves an X display.
//!
//! [`Desktop`] is what a backend implements and the GUI engine drives. [`X11Desktop`] is the backend over an X display;
//! it connects by display name, or over the stream a [`Connector`] hands it, so a display on another machine (an SSH
//! channel) is served by the same code. [`source::DesktopOf`] offers any [`Desktop`] as a `bohio_desktop` source.

#![forbid(unsafe_code)]

pub mod desktop;
#[cfg(all(target_os = "linux", feature = "logind"))]
pub mod logind;
pub mod source;
mod x11;
mod x11_clipboard;

pub use desktop::{Capabilities, Desktop, DesktopState, Frame, GuiError};
pub use x11::{
    Connector, Wire, WireStream, X11Desktop, X11Settings, display_id, keysym_of, one_primary,
};
