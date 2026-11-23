//! A desktop across the kind: an application's or the platform's [`Desktop`] offered as a [`DesktopSource`], and a
//! [`DesktopSession`] of any source used as the [`Desktop`] the GUI engine drives.
//!
//! The two directions are the same conversions read the other way, so a desktop that goes in as a source and comes out
//! as a session is the desktop it was.

#![forbid(unsafe_code)]

use std::sync::Arc;

use bohio_api::guatiao::Status;
use bohio_api::guatiao::library::{Object, ProviderError};
use bohio_api::model::{DisplayModel, FrameHeader, InputModel, RectModel, SourceInfo, StateModel};
use bohio_api::{DesktopSession, DesktopSource, vocab};
use bohio_proto::ErrorCode;
use bohio_proto::gui::{Button, ButtonAction, Display, DisplayId, InputEvent, KeyAction, Rect};

use super::Source;
use crate::gui::{Capabilities, Desktop, DesktopState, Frame, GuiError};

/// The error a provider's refusal is, for a peer.
pub fn gui_error(error: &ProviderError) -> GuiError {
    GuiError::new(code_of(error.status), error.message().to_string())
}

/// The wire code a provider status is reported as.
pub fn code_of(status: Status) -> ErrorCode {
    match status {
        Status::GUATIAO_ERR_UNSUPPORTED => ErrorCode::Unsupported,
        Status::GUATIAO_ERR_NOT_FOUND => ErrorCode::NotFound,
        Status::GUATIAO_ERR_BAD_VALUE | Status::GUATIAO_ERR_WRONG_KIND => ErrorCode::BadRequest,
        Status::GUATIAO_ERR_BUSY => ErrorCode::Busy,
        Status::GUATIAO_ERR_GONE | Status::GUATIAO_ERR_NULL => ErrorCode::Unavailable,
        _ => ErrorCode::Io,
    }
}

/// The status a wire code is handed to a provider as, the nearest one guatiao has.
fn status_of(code: ErrorCode) -> Status {
    match code {
        ErrorCode::Unsupported => Status::GUATIAO_ERR_UNSUPPORTED,
        ErrorCode::NotFound => Status::GUATIAO_ERR_NOT_FOUND,
        ErrorCode::BadRequest => Status::GUATIAO_ERR_BAD_VALUE,
        ErrorCode::Busy => Status::GUATIAO_ERR_BUSY,
        ErrorCode::Unavailable => Status::GUATIAO_ERR_GONE,
        _ => Status::GUATIAO_ERR_INTERNAL,
    }
}

fn provider_error(error: &GuiError) -> ProviderError {
    ProviderError::new(status_of(error.code), &error.message)
}

fn rect_model(rect: Rect) -> RectModel {
    RectModel {
        x: u64::from(rect.x),
        y: u64::from(rect.y),
        width: u64::from(rect.width),
        height: u64::from(rect.height),
    }
}

fn rect_of(model: RectModel) -> Result<Rect, GuiError> {
    let narrow = |value: u64| {
        u32::try_from(value).map_err(|_| GuiError::bad_request("the region is out of range"))
    };
    Ok(Rect {
        x: narrow(model.x)?,
        y: narrow(model.y)?,
        width: narrow(model.width)?,
        height: narrow(model.height)?,
    })
}

fn display_model(display: &Display) -> DisplayModel {
    DisplayModel {
        id: display.id.0.clone(),
        name: display.name.clone(),
        primary: display.primary,
        x: i64::from(display.x),
        y: i64::from(display.y),
        width: u64::from(display.width),
        height: u64::from(display.height),
        scale: display.scale,
        rotation: display.rotation.map(u64::from),
    }
}

fn display_of(model: DisplayModel) -> Display {
    Display {
        id: DisplayId(model.id),
        name: model.name,
        primary: model.primary,
        x: i32::try_from(model.x).unwrap_or(0),
        y: i32::try_from(model.y).unwrap_or(0),
        width: u32::try_from(model.width).unwrap_or(0),
        height: u32::try_from(model.height).unwrap_or(0),
        scale: model.scale,
        rotation: model
            .rotation
            .and_then(|degrees| u32::try_from(degrees).ok()),
    }
}

fn button_name(button: Button) -> &'static str {
    match button {
        Button::Left => "left",
        Button::Right => "right",
        Button::Middle => "middle",
        Button::Back => "back",
        Button::Forward => "forward",
    }
}

fn button_of(name: &str) -> Option<Button> {
    Some(match name {
        "left" => Button::Left,
        "right" => Button::Right,
        "middle" => Button::Middle,
        "back" => Button::Back,
        "forward" => Button::Forward,
        _ => return None,
    })
}

fn button_action_name(action: ButtonAction) -> &'static str {
    match action {
        ButtonAction::Press => "press",
        ButtonAction::Release => "release",
        ButtonAction::Click => "click",
    }
}

fn key_action_name(action: KeyAction) -> &'static str {
    match action {
        KeyAction::Press => "press",
        KeyAction::Release => "release",
        KeyAction::Click => "click",
    }
}

fn input_model(event: &InputEvent) -> Option<InputModel> {
    let display = |id: &Option<DisplayId>| id.as_ref().map(|id| id.0.clone());
    Some(match event {
        InputEvent::Move { display: id, x, y } => InputModel {
            kind: vocab::input::MOVE.to_owned(),
            display: display(id),
            x: Some(i64::from(*x)),
            y: Some(i64::from(*y)),
            ..InputModel::default()
        },
        InputEvent::Button {
            button,
            action,
            count,
            display: id,
            x,
            y,
        } => InputModel {
            kind: vocab::input::BUTTON.to_owned(),
            display: display(id),
            x: x.map(i64::from),
            y: y.map(i64::from),
            button: Some(button_name(*button).to_owned()),
            action: Some(button_action_name(*action).to_owned()),
            count: count.map(u64::from),
            ..InputModel::default()
        },
        InputEvent::Scroll {
            display: id,
            x,
            y,
            dx,
            dy,
        } => InputModel {
            kind: vocab::input::SCROLL.to_owned(),
            display: display(id),
            x: x.map(i64::from),
            y: y.map(i64::from),
            dx: i64::from(*dx),
            dy: i64::from(*dy),
            ..InputModel::default()
        },
        InputEvent::Key { key, action } => InputModel {
            kind: vocab::input::KEY.to_owned(),
            key: Some(key.clone()),
            action: Some(key_action_name(*action).to_owned()),
            ..InputModel::default()
        },
        InputEvent::Text { text } => InputModel {
            kind: vocab::input::TEXT.to_owned(),
            text: Some(text.clone()),
            ..InputModel::default()
        },
        // A pause is the engine's: it never reaches a desktop.
        _ => return None,
    })
}

fn input_event(model: InputModel) -> Result<InputEvent, GuiError> {
    let narrow = |value: Option<i64>| {
        value
            .map(|value| {
                i32::try_from(value)
                    .map_err(|_| GuiError::bad_request("a coordinate is out of range"))
            })
            .transpose()
    };
    let display = model.display.clone().map(DisplayId);
    Ok(match model.kind.as_str() {
        vocab::input::MOVE => InputEvent::Move {
            display,
            x: narrow(model.x)?.unwrap_or(0),
            y: narrow(model.y)?.unwrap_or(0),
        },
        vocab::input::BUTTON => InputEvent::Button {
            button: model
                .button
                .as_deref()
                .and_then(button_of)
                .ok_or_else(|| GuiError::bad_request("not a button"))?,
            action: match model.action.as_deref() {
                Some("press") => ButtonAction::Press,
                Some("release") => ButtonAction::Release,
                _ => ButtonAction::Click,
            },
            count: model.count.and_then(|count| u32::try_from(count).ok()),
            display,
            x: narrow(model.x)?,
            y: narrow(model.y)?,
        },
        vocab::input::SCROLL => InputEvent::Scroll {
            display,
            x: narrow(model.x)?,
            y: narrow(model.y)?,
            dx: i32::try_from(model.dx).unwrap_or(0),
            dy: i32::try_from(model.dy).unwrap_or(0),
        },
        vocab::input::KEY => InputEvent::Key {
            key: model
                .key
                .ok_or_else(|| GuiError::bad_request("a key event names no key"))?,
            action: match model.action.as_deref() {
                Some("press") => KeyAction::Press,
                Some("release") => KeyAction::Release,
                _ => KeyAction::Click,
            },
        },
        vocab::input::TEXT => InputEvent::Text {
            text: model.text.unwrap_or_default(),
        },
        _ => {
            return Err(GuiError::unsupported(
                "this event is not one a desktop takes",
            ));
        }
    })
}

// ---------------------------------------------------------------------------------------------------------------
// A Desktop offered as a source.

/// A [`Desktop`] an application or the platform gives, offered as a [`DesktopSource`].
pub struct DesktopOf {
    desktop: Arc<dyn Desktop>,
}

impl DesktopOf {
    /// `desktop` as a source.
    pub fn new(desktop: Arc<dyn Desktop>) -> Self {
        Self { desktop }
    }
}

impl DesktopSource for DesktopOf {
    fn describe(&self) -> Result<SourceInfo, ProviderError> {
        let capabilities = self.desktop.capabilities();
        let state = self.desktop.state();
        let mut offers = vec![vocab::desktop::CAPTURE];
        let mut lacks = Vec::new();
        for (word, has) in [
            (vocab::desktop::INPUT, capabilities.input),
            (vocab::desktop::CLIPBOARD, capabilities.clipboard),
            (vocab::desktop::LOCK_DETECTION, capabilities.lock_detection),
        ] {
            if has {
                offers.push(word);
            } else {
                lacks.push(word);
            }
        }
        if state.as_ref().is_ok_and(|state| state.displays.len() > 1) {
            offers.push(vocab::desktop::MULTI_DISPLAY);
        } else {
            lacks.push(vocab::desktop::MULTI_DISPLAY);
        }
        let info = SourceInfo::new(&capabilities.backend, &offers, &lacks);
        Ok(match state {
            Ok(_) => info,
            Err(error) => info.unavailable(&error.message),
        })
    }

    fn open(&self) -> Result<Object<dyn DesktopSession>, ProviderError> {
        self.desktop
            .state()
            .map_err(|error| provider_error(&error))?;
        Ok(SessionOf {
            desktop: Arc::clone(&self.desktop),
        }
        .into_object())
    }
}

struct SessionOf {
    desktop: Arc<dyn Desktop>,
}

impl SessionOf {
    fn region(
        &self,
        display: &str,
        region: Option<RectModel>,
    ) -> Result<(DisplayId, Option<Rect>, u64, u64), ProviderError> {
        let id = DisplayId(display.to_owned());
        let state = self
            .desktop
            .state()
            .map_err(|error| provider_error(&error))?;
        let shown = state
            .display(Some(&id))
            .map_err(|error| provider_error(&error))?;
        let region = region
            .map(|model| rect_of(model).map_err(|error| provider_error(&error)))
            .transpose()?;
        let (width, height) = region
            .map_or((u64::from(shown.width), u64::from(shown.height)), |rect| {
                (u64::from(rect.width), u64::from(rect.height))
            });
        Ok((id, region, width, height))
    }
}

impl DesktopSession for SessionOf {
    fn state(&self) -> Result<StateModel, ProviderError> {
        let state = self
            .desktop
            .state()
            .map_err(|error| provider_error(&error))?;
        Ok(StateModel {
            user: state.user,
            active: state.active,
            locked: state.locked,
            displays: state.displays.iter().map(display_model).collect(),
        })
    }

    fn frame_size(
        &self,
        display: &str,
        region: Option<RectModel>,
    ) -> Result<FrameHeader, ProviderError> {
        let (_, _, width, height) = self.region(display, region)?;
        Ok(FrameHeader { width, height })
    }

    fn capture(
        &self,
        display: &str,
        region: Option<RectModel>,
        dst: &mut [u8],
    ) -> Result<FrameHeader, ProviderError> {
        let (id, region, _, _) = self.region(display, region)?;
        let frame = self
            .desktop
            .capture(&id, region)
            .map_err(|error| provider_error(&error))?;
        let header = FrameHeader {
            width: u64::from(frame.width()),
            height: u64::from(frame.height()),
        };
        let target = dst.get_mut(..frame.rgb().len()).ok_or_else(|| {
            ProviderError::new(
                Status::GUATIAO_ERR_INTERNAL,
                "the frame is larger than its size said",
            )
        })?;
        target.copy_from_slice(frame.rgb());
        Ok(header)
    }

    fn input(&self, events: Vec<InputModel>) -> Result<u64, ProviderError> {
        let events: Vec<InputEvent> = events
            .into_iter()
            .map(input_event)
            .collect::<Result<_, _>>()
            .map_err(|error| provider_error(&error))?;
        self.desktop
            .input(&events)
            .map(u64::from)
            .map_err(|error| provider_error(&error))
    }

    fn cursor(&self) -> Result<Option<InputModel>, ProviderError> {
        Ok(self.desktop.cursor().map(|(display, x, y)| InputModel {
            kind: vocab::input::MOVE.to_owned(),
            display: Some(display.0),
            x: Some(i64::from(x)),
            y: Some(i64::from(y)),
            ..InputModel::default()
        }))
    }

    fn clipboard_text(&self) -> Result<String, ProviderError> {
        self.desktop
            .clipboard_text()
            .map_err(|error| provider_error(&error))
    }

    fn set_clipboard_text(&self, text: &str) -> Result<(), ProviderError> {
        self.desktop
            .set_clipboard_text(text)
            .map_err(|error| provider_error(&error))
    }
}

// ---------------------------------------------------------------------------------------------------------------
// A session of any source, driven as the Desktop the GUI engine knows.

/// An open session of a desktop source, as the [`Desktop`] the engine drives. Every call first asks whether the source
/// is still the one the node was configured with: a reconfigured source ends what ran on the old one.
pub struct KindDesktop {
    source: Source<dyn DesktopSource>,
    session: Object<dyn DesktopSession>,
}

impl KindDesktop {
    /// Opens a session on `source`.
    ///
    /// # Errors
    ///
    /// The source's refusal (`unavailable` when it cannot be reached, with its reason).
    pub fn open(source: Source<dyn DesktopSource>) -> Result<Self, GuiError> {
        let session = source.get().open().map_err(|error| gui_error(&error))?;
        Ok(Self { source, session })
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
        Ok(DesktopState {
            user: model.user,
            active: model.active,
            locked: model.locked,
            displays: model.displays.into_iter().map(display_of).collect(),
        })
    }

    fn capture(&self, display: &DisplayId, region: Option<Rect>) -> Result<Frame, GuiError> {
        self.live()?;
        let region = region.map(rect_model);
        let header = self
            .session
            .frame_size(&display.0, region)
            .map_err(|error| gui_error(&error))?;
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
            Ok(info) => Capabilities {
                input: !info.lacks(vocab::desktop::INPUT),
                clipboard: !info.lacks(vocab::desktop::CLIPBOARD),
                lock_detection: !info.lacks(vocab::desktop::LOCK_DETECTION),
                backend: info.provider,
            },
            Err(_) => Capabilities::default(),
        }
    }
}
