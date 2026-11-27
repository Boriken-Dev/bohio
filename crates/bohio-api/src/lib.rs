// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The provider kinds of Bohío.
//!
//! A node reaches a system through a **source**: a configured instance of a **provider**. A provider serves a
//! **capability**, and each capability is one `guatiao` kind: [`TerminalSource`] (a console), [`DesktopSource`]
//! (a screen with input), [`PowerSource`] and [`MediaSource`] (virtual media). The host (`bohio`) and every
//! provider compile against this crate and nothing else of Bohío's; it re-exports [`guatiao`], and a provider
//! names guatiao only through it, so the two sides cannot drift to different versions.
//!
//! `native`, the node's own pseudo-terminal and screen, is a provider of these kinds like any other.
//!
//! Every optional method has a default body that answers `unsupported`, and a kind grows only by appending
//! methods with a default body after the existing ones. [`contract`] holds the suite every provider of a kind
//! passes, and `bohio-api.kinds.json` describes the kinds for a provider written in another language.

#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub use guatiao;

pub mod contract;
pub mod desktop;
pub mod exec;
pub mod files;
pub mod interval;
pub mod media;
pub mod model;
pub mod power;
pub mod terminal;
#[cfg(feature = "testing")]
#[cfg_attr(docsrs, doc(cfg(feature = "testing")))]
pub mod testing;
pub mod vocab;

pub use desktop::{DesktopSession, DesktopSource};
pub use exec::{ExecSource, Process};
pub use files::{DirReader, Directory, File, FilesSource};
pub use interval::Interval;
pub use media::{BlockReader, MediaSource};
pub use model::{
    Delivery, DirEntryModel, DisplayModel, EnvVar, ExecStart, FileStat, FrameHeader, InputModel,
    MediaSlot, OpenModel, PowerState, ProgramSpec, RectModel, SourceInfo, StateModel, TerminalExit,
    TerminalOpen, TimeModel, VfsStat,
};
pub use power::PowerSource;
pub use terminal::{Terminal, TerminalSource};

use guatiao::Status;
use guatiao::library::ProviderError;

/// The answer of a method a provider does not implement: the one every default body gives, which a host
/// reports to a peer as `unsupported`.
#[must_use]
pub fn unsupported(what: &str) -> ProviderError {
    ProviderError::new(Status::GUATIAO_ERR_UNSUPPORTED, what)
}

/// A refusal because the source cannot be used now (a device that is gone, a service that does not answer): status
/// `GONE`, which the
/// host reports it as `unavailable`, with this text.
#[must_use]
pub fn unavailable(why: &str) -> ProviderError {
    ProviderError::new(Status::GUATIAO_ERR_GONE, why)
}
