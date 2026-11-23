// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The desktop kind: a screen a peer looks at and types at.

use guatiao::library::{Object, ProviderError};

use crate::model::{FrameHeader, InputModel, RectModel, SourceInfo, StateModel};
use crate::unsupported;

/// A source of desktop sessions.
#[guatiao::kind(name = "bohio_desktop")]
pub trait DesktopSource: Send + Sync {
    /// What this source is and can do now; asked at every `info`, never cached by the host.
    ///
    /// # Errors
    ///
    /// Only a source that cannot even describe itself.
    fn describe(&self) -> Result<SourceInfo, ProviderError>;

    /// Opens a session on the desktop.
    ///
    /// # Errors
    ///
    /// `GONE` when there is no desktop to reach (the reason is the message).
    fn open(&self) -> Result<Object<dyn DesktopSession>, ProviderError>;
}

/// One session on a desktop. The host calls it one call at a time, after its own checks of the grant, the display,
/// the region and the rates.
#[guatiao::kind(object, name = "bohio_desktop_session")]
pub trait DesktopSession: Send + Sync {
    /// The session's state: who owns it, whether it is at the screen and locked, and its displays.
    ///
    /// # Errors
    ///
    /// `GONE` when there is no desktop to describe.
    fn state(&self) -> Result<StateModel, ProviderError>;

    /// The size of the frame `capture` of this display and region will write; the host sizes its buffer from it.
    ///
    /// # Errors
    ///
    /// `NOT_FOUND` for a display the desktop does not have, `GONE`.
    fn frame_size(
        &self,
        display: &str,
        region: Option<RectModel>,
    ) -> Result<FrameHeader, ProviderError>;

    /// Writes a picture of `region` of `display` (all of it when absent) into `dst`, RGB, three bytes a pixel, rows
    /// from the top, no padding, and answers its size. `dst` is at least what `frame_size` said.
    ///
    /// # Errors
    ///
    /// `GONE` when the session is not at the screen, `INTERNAL`.
    fn capture(
        &self,
        display: &str,
        region: Option<RectModel>,
        dst: &mut [u8],
    ) -> Result<FrameHeader, ProviderError>;

    /// Applies the events in order and answers how many it took.
    ///
    /// # Errors
    ///
    /// `GONE` when input is blocked, `UNSUPPORTED` for an event the source cannot make.
    fn input(&self, events: Vec<InputModel>) -> Result<u64, ProviderError> {
        let _ = events;
        Err(unsupported("this desktop takes no input"))
    }

    /// The pointer's display and position (an `InputModel` of kind `move`), when the source can tell.
    ///
    /// # Errors
    ///
    /// Never, by default.
    fn cursor(&self) -> Result<Option<InputModel>, ProviderError> {
        Ok(None)
    }

    /// The clipboard's text.
    ///
    /// # Errors
    ///
    /// `UNSUPPORTED` when the source has no clipboard, `GONE` when it holds something that is not text.
    fn clipboard_text(&self) -> Result<String, ProviderError> {
        Err(unsupported("this desktop has no clipboard"))
    }

    /// Replaces the clipboard's text.
    ///
    /// # Errors
    ///
    /// As `clipboard_text`.
    fn set_clipboard_text(&self, text: &str) -> Result<(), ProviderError> {
        let _ = text;
        Err(unsupported("this desktop has no clipboard"))
    }
}
