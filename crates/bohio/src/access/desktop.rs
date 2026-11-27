//! A desktop across the kind: an open session of a desktop source, as the [`Desktop`] the GUI engine drives.

#![forbid(unsafe_code)]

use bohio_api::guatiao::library::Object;
use bohio_api::model::InputModel;
use bohio_api::{DesktopSession, DesktopSource, vocab};
use bohio_proto::ErrorCode;
use bohio_proto::gui::{DisplayId, InputEvent, Rect};
use bohio_x11::source::{display_of, gui_error, input_model, rect_model};

use super::Source;
use crate::gui::{Capabilities, Desktop, DesktopState, Frame, GuiError};

// A session of any source, driven as the Desktop the GUI engine knows.

/// An open session of a desktop source, as the [`Desktop`] the engine drives. Every call first asks whether the source
/// is still the one the node was configured with: a reconfigured source ends what ran on the old one.
pub struct KindDesktop {
    source: Source<dyn DesktopSource>,
    session: Object<dyn DesktopSession>,
    /// The most pixels a frame may hold; `None` until [`KindDesktop::bounded`] says, which refuses a whole-display capture.
    max_pixels: Option<u64>,
}

impl KindDesktop {
    /// Opens a session on `source`.
    ///
    /// # Errors
    ///
    /// The source's refusal (`unavailable` when it cannot be reached, with its reason).
    pub fn open(source: Source<dyn DesktopSource>) -> Result<Self, GuiError> {
        let session = source.get().open().map_err(|error| gui_error(&error))?;
        Ok(Self {
            source,
            session,
            max_pixels: None,
        })
    }

    /// Bounds every frame to `max_pixels` (the host's `[gui] max_snapshot_pixels`): the provider's own word for the size of
    /// a frame sizes the buffer only when it is no larger than the region asked, and never above this.
    #[must_use]
    pub const fn bounded(mut self, max_pixels: u64) -> Self {
        self.max_pixels = Some(max_pixels);
        self
    }

    fn live(&self) -> Result<(), GuiError> {
        if self.source.is_current() {
            Ok(())
        } else {
            Err(GuiError::unavailable("the source was reconfigured"))
        }
    }
}

impl Desktop for KindDesktop {
    fn state(&self) -> Result<DesktopState, GuiError> {
        self.live()?;
        let model = self.session.state().map_err(|error| gui_error(&error))?;
        let mut state = DesktopState::new(
            model.active,
            model.locked,
            model.displays.into_iter().map(display_of).collect(),
        );
        state.user = model.user;
        Ok(state)
    }

    fn capture(&self, display: &DisplayId, region: Option<Rect>) -> Result<Frame, GuiError> {
        self.live()?;
        let region = region.map(rect_model);
        let header = self
            .session
            .frame_size(&display.0, region)
            .map_err(|error| gui_error(&error))?;
        let allowed = match (region, self.max_pixels) {
            (Some(asked), Some(most)) => asked.width.saturating_mul(asked.height).min(most),
            (Some(asked), None) => asked.width.saturating_mul(asked.height),
            (None, Some(most)) => most,
            (None, None) => {
                return Err(GuiError::new(
                    ErrorCode::Internal,
                    "no bound on the frame of a whole display",
                ));
            }
        };
        if header.width.saturating_mul(header.height) > allowed {
            return Err(GuiError::new(
                ErrorCode::Internal,
                "the provider sized a frame larger than the region asked",
            ));
        }
        let len = usize::try_from(header.len())
            .map_err(|_| GuiError::new(ErrorCode::Internal, "the frame does not fit memory"))?;
        let mut rgb = vec![0_u8; len];
        let written = self
            .session
            .capture(&display.0, region, &mut rgb)
            .map_err(|error| gui_error(&error))?;
        let (width, height) = (
            u32::try_from(written.width).unwrap_or(0),
            u32::try_from(written.height).unwrap_or(0),
        );
        rgb.truncate(usize::try_from(written.len()).unwrap_or(0));
        Frame::new(width, height, rgb)
    }

    fn input(&self, events: &[InputEvent]) -> Result<u32, GuiError> {
        self.live()?;
        let models: Vec<InputModel> = events.iter().filter_map(input_model).collect();
        self.session
            .input(models)
            .map(|taken| u32::try_from(taken).unwrap_or(u32::MAX))
            .map_err(|error| gui_error(&error))
    }

    fn cursor(&self) -> Option<(DisplayId, i32, i32)> {
        self.live().ok()?;
        let at = self.session.cursor().ok()??;
        Some((
            DisplayId(at.display?),
            i32::try_from(at.x?).ok()?,
            i32::try_from(at.y?).ok()?,
        ))
    }

    fn clipboard_text(&self) -> Result<String, GuiError> {
        self.live()?;
        self.session
            .clipboard_text()
            .map_err(|error| gui_error(&error))
    }

    fn set_clipboard_text(&self, text: &str) -> Result<(), GuiError> {
        self.live()?;
        self.session
            .set_clipboard_text(text)
            .map_err(|error| gui_error(&error))
    }

    fn capabilities(&self) -> Capabilities {
        match self.source.get().describe() {
            Ok(info) => Capabilities::new(
                info.provider.clone(),
                !info.lacks(vocab::desktop::INPUT),
                !info.lacks(vocab::desktop::CLIPBOARD),
                !info.lacks(vocab::desktop::LOCK_DETECTION),
            ),
            Err(_) => Capabilities::default(),
        }
    }
}
