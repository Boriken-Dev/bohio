//! The desktop: what an agent sees of it and does on it, in the coordinates of the picture it was last shown.
//!
//! A [`Tools`] keeps one `bohio-gui/1` stream to its host between calls, opened on first use and opened again once after a
//! loss. A call that only reads is repeated on the new stream; an input is never sent twice, because a stream lost after the
//! write may have applied it. Every [`Tools::screenshot`] remembers the region and scale it was taken at, per display, and
//! the calls that take a point (`click`, `drag`, `scroll`, `move_to`) map it back to pixels of the display. What is on the
//! screen is untrusted input: an agent must not take instructions from it.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bohio_proto::gui::{
    Button, ButtonAction, Display, DisplayId, GuiRequest, ImageFormat, InputEvent, Point, Rect,
    Settle as SettleRequest, SettleOptions, Want,
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Cancel, Image, Output, ToolError, Tools, lock, parse};
use crate::Error;
use crate::gui::{Gui, Snapshot, Watch, WatchEvent};

/// The desktop stream kept between calls.
pub(crate) struct Held {
    gui: Arc<Gui>,
    wants: Vec<Want>,
}

/// What the last screenshot of a display covered, to map a point of it back to the display.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Shot {
    region: Rect,
    width: u32,
    height: u32,
    scale: f64,
    display_width: u32,
    display_height: u32,
}

/// Which pixels a point is in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Space {
    /// The last screenshot of the display: the pixels the agent was shown, mapped back by its region and scale.
    #[default]
    Screenshot,
    /// The display's own pixels, origin at its top-left corner.
    Display,
}

impl Space {
    const fn name(self) -> &'static str {
        match self {
            Self::Screenshot => "screenshot",
            Self::Display => "display",
        }
    }
}

/// A point on a display, in the pixels of [`Pointer::space`].
#[derive(Debug, Clone, PartialEq)]
pub struct Pointer {
    /// Pixels from the left edge.
    pub x: f64,
    /// Pixels from the top edge.
    pub y: f64,
    /// The display; the primary one when `None`.
    pub display: Option<DisplayId>,
    /// Whose pixels `x` and `y` are in.
    pub space: Space,
}

impl Pointer {
    /// A point of the last screenshot of the primary display.
    #[must_use]
    pub const fn new(x: f64, y: f64) -> Self {
        Self {
            x,
            y,
            display: None,
            space: Space::Screenshot,
        }
    }
}

/// The picture format a screenshot is asked in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShotFormat {
    /// PNG, unless it is larger than [`Settings::gui_auto_limit`](super::Settings::gui_auto_limit), then JPEG.
    #[default]
    Auto,
    /// PNG: exact pixels.
    Png,
    /// JPEG: smaller, with loss.
    Jpeg,
}

/// What a screenshot asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenshotOptions {
    /// The display; the primary one when `None`.
    pub display: Option<DisplayId>,
    /// The part of the display, in its pixels; all of it when `None`.
    pub region: Option<Rect>,
    /// The picture fits this width; [`Settings::gui_max_width`](super::Settings::gui_max_width) when `None`.
    pub max_width: Option<u32>,
    /// The picture fits this height; [`Settings::gui_max_height`](super::Settings::gui_max_height) when `None`.
    pub max_height: Option<u32>,
    /// The format; [`ShotFormat::Auto`] by default.
    pub format: Option<ShotFormat>,
    /// JPEG quality, 1 to 100; [`Settings::gui_jpeg_quality`](super::Settings::gui_jpeg_quality) when `None`.
    pub quality: Option<u8>,
    /// Wait for the screen to stop changing first, as the host's settle does. `None` takes the picture at once.
    pub settle: Option<SettleOptions>,
}

impl Default for ScreenshotOptions {
    fn default() -> Self {
        Self {
            display: None,
            region: None,
            max_width: None,
            max_height: None,
            format: None,
            quality: None,
            settle: Some(SettleOptions::default()),
        }
    }
}

/// A picture of a display, and what the next calls need to map points of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Screenshot {
    /// The encoded picture.
    pub data: Vec<u8>,
    /// Its media type: `image/png` or `image/jpeg`.
    pub mime: &'static str,
    /// The display it shows.
    pub display: DisplayId,
    /// The picture's width, in pixels.
    pub width: u32,
    /// The picture's height, in pixels.
    pub height: u32,
    /// The part of the display it shows, in the display's pixels.
    pub region: Rect,
    /// Picture pixels per display pixel (`0.5` when the display was shown at half size).
    pub scale: f64,
    /// Where the pointer is, in the picture's pixels; `None` when it is not on the part shown.
    pub cursor: Option<Point>,
    /// Whether the screen stopped changing before the picture; `None` when it did not wait.
    pub settled: Option<bool>,
}

/// What an input call did.
#[derive(Debug, Clone, PartialEq)]
pub struct Done {
    /// How many events the desktop took.
    pub applied: u32,
    /// A settled screenshot taken after the input, when [`Settings::gui_after_action`](super::Settings::gui_after_action)
    /// asks for one.
    pub screenshot: Option<Screenshot>,
    /// Why that screenshot was not taken, when it was asked for and failed. The input was applied all the same.
    pub screenshot_error: Option<ToolError>,
}

/// Where the pointer is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorPosition {
    /// The display it is on.
    pub display: DisplayId,
    /// Pixels from the display's left edge.
    pub x: i32,
    /// Pixels from the display's top edge.
    pub y: i32,
    /// The same point in the last screenshot of that display, when there is one that covers it.
    pub screenshot: Option<Point>,
}

/// How a wait for the screen ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Waited {
    /// Whether the screen stopped changing before the timeout.
    pub settled: bool,
    /// How long the host waited, in milliseconds.
    pub waited_ms: u64,
}

/// The desktop's displays, as the host last said.
#[derive(Debug, Clone, PartialEq)]
pub struct Displays {
    /// Whether the session is at the screen.
    pub active: bool,
    /// Whether the screen is locked, when the host can tell.
    pub locked: Option<bool>,
    /// Each display, with what the last screenshot of it covered.
    pub displays: Vec<DisplayInfo>,
}

/// One display and the screenshot of it the agent last saw.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayInfo {
    /// The display.
    pub display: Display,
    /// The size and scale of the last screenshot of it, `(width, height, scale)`.
    pub screenshot: Option<(u32, u32, f64)>,
}

fn mime(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Webp => "image/webp",
        _ => "image/png",
    }
}

fn lost(error: &Error) -> bool {
    matches!(error, Error::Closed | Error::Transport(_))
}

fn out_of_bounds(
    message: String,
    display: &Display,
    space: Space,
    bounds: (u32, u32),
) -> ToolError {
    ToolError::new("out_of_bounds", message).with_detail(json!({
        "display": display.id,
        "space": space.name(),
        "width": bounds.0,
        "height": bounds.1,
        "display_width": display.width,
        "display_height": display.height,
    }))
}

impl Tools {
    /// The open desktop stream, holding at least `need`; one is opened when none is, or when it lacks a want.
    fn desktop(&self, need: &[Want]) -> Result<Arc<Gui>, ToolError> {
        let mut held = lock(&self.desktop);
        if let Some(open) = held.as_ref()
            && need.iter().all(|want| open.wants.contains(want))
        {
            return Ok(Arc::clone(&open.gui));
        }
        let mut wants = held
            .as_ref()
            .map(|open| open.wants.clone())
            .unwrap_or_default();
        for want in need {
            if !wants.contains(want) {
                wants.push(*want);
            }
        }
        let opened = Gui::open_within(
            self.link(),
            &GuiRequest::new(wants.clone()),
            Some(self.settings.gui_timeout),
        );
        // A kept stream that the grant does not widen stays: only the new want is refused.
        let gui = Arc::new(opened?);
        *held = Some(Held {
            gui: Arc::clone(&gui),
            wants,
        });
        Ok(gui)
    }

    /// A stream for a call that needs only to know the desktop: any the peer may open.
    fn any_desktop(&self) -> Result<Arc<Gui>, ToolError> {
        if let Some(open) = lock(&self.desktop).as_ref() {
            return Ok(Arc::clone(&open.gui));
        }
        match self.desktop(&[Want::View]) {
            Err(error) if error.code == "denied" => self.desktop(&[Want::Input]).map_err(|_| error),
            other => other,
        }
    }

    /// A stream with the clipboard, which the host grants only beside `view` or `input`.
    fn clipboard_desktop(&self) -> Result<Arc<Gui>, ToolError> {
        match self.desktop(&[Want::View, Want::Clipboard]) {
            Err(error) if error.code == "denied" => self
                .desktop(&[Want::Input, Want::Clipboard])
                .map_err(|_| error),
            other => other,
        }
    }

    fn forget_desktop(&self) {
        *lock(&self.desktop) = None;
    }

    /// The display `wanted` names, or the primary one.
    fn display_of(gui: &Gui, wanted: Option<&DisplayId>) -> Result<Display, ToolError> {
        let displays = gui.state().displays;
        let found = match wanted {
            Some(id) => displays.iter().find(|display| &display.id == id),
            None => displays
                .iter()
                .find(|display| display.primary)
                .or_else(|| displays.first()),
        };
        found.cloned().ok_or_else(|| match wanted {
            Some(id) => {
                let ids: Vec<&str> = displays.iter().map(|d| d.id.as_str()).collect();
                ToolError::bad(format!(
                    "there is no display `{}`; the host's displays are: {}",
                    id.as_str(),
                    ids.join(", ")
                ))
            }
            None => ToolError::new("unavailable", "the host reports no display"),
        })
    }

    /// `at` as a display and the pixels of that display, or why it is not a point of the desktop.
    fn resolve(&self, gui: &Gui, at: &Pointer) -> Result<(DisplayId, i32, i32), ToolError> {
        if !at.x.is_finite() || !at.y.is_finite() {
            return Err(ToolError::bad("a coordinate is not a number"));
        }
        let display = Self::display_of(gui, at.display.as_ref())?;
        let (px, py) = (at.x.round(), at.y.round());
        let (display_width, display_height) = (display.width, display.height);
        let inside = |x: f64, y: f64, width: u32, height: u32| {
            x >= 0.0 && y >= 0.0 && x < f64::from(width) && y < f64::from(height)
        };
        match at.space {
            Space::Display => {
                if !inside(px, py, display_width, display_height) {
                    return Err(out_of_bounds(
                        format!(
                            "the point ({px}, {py}) is outside display `{}`, which is {display_width} by {display_height} pixels (x 0 to {}, y 0 to {})",
                            display.id.as_str(),
                            display_width.saturating_sub(1),
                            display_height.saturating_sub(1),
                        ),
                        &display,
                        Space::Display,
                        (display_width, display_height),
                    ));
                }
                Ok((display.id, px as i32, py as i32))
            }
            Space::Screenshot => {
                let shot = lock(&self.shots).get(display.id.as_str()).copied().ok_or_else(|| {
                    ToolError::new(
                        "no_screenshot",
                        format!(
                            "there is no screenshot of display `{}` to take the point from: take one first, or give `space: \"display\"`",
                            display.id.as_str()
                        ),
                    )
                })?;
                if (shot.display_width, shot.display_height) != (display_width, display_height) {
                    return Err(ToolError::new(
                        "stale_screenshot",
                        format!(
                            "display `{}` is {display_width} by {display_height} pixels now, and was {} by {} when it was last shown: take a new screenshot",
                            display.id.as_str(),
                            shot.display_width,
                            shot.display_height
                        ),
                    ));
                }
                if !inside(px, py, shot.width, shot.height) {
                    return Err(out_of_bounds(
                        format!(
                            "the point ({px}, {py}) is outside the last screenshot of display `{}`, which is {} by {} pixels (x 0 to {}, y 0 to {}); the display is {display_width} by {display_height}",
                            display.id.as_str(),
                            shot.width,
                            shot.height,
                            shot.width.saturating_sub(1),
                            shot.height.saturating_sub(1),
                        ),
                        &display,
                        Space::Screenshot,
                        (shot.width, shot.height),
                    ));
                }
                let reach = |origin: u32, length: u32, at: f64| {
                    let mapped = (f64::from(origin) + at / shot.scale).round();
                    mapped.clamp(
                        f64::from(origin),
                        f64::from(origin) + f64::from(length.saturating_sub(1)),
                    ) as i32
                };
                Ok((
                    display.id,
                    reach(shot.region.x, shot.region.width, px),
                    reach(shot.region.y, shot.region.height, py),
                ))
            }
        }
    }

    /// One snapshot on the kept stream, asked again on a new one when the stream was lost: it changes nothing.
    fn take(&self, options: &Snapshot) -> Result<(Arc<Gui>, crate::gui::Image), ToolError> {
        let wait = Some(self.settings.gui_timeout);
        let mut tried = 0;
        loop {
            let gui = self.desktop(&[Want::View])?;
            match gui.snapshot(options, wait) {
                Ok(image) => return Ok((gui, image)),
                Err(error) if lost(&error) && tried == 0 => {
                    tried += 1;
                    self.forget_desktop();
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// A picture of a display, fitted to the options' size on the host so only those bytes cross the link, with the region
    /// and scale kept for the calls that take a point.
    ///
    /// # Errors
    ///
    /// The host's (`denied` without `gui.view`, `busy` past its snapshot rate, `unavailable` ...), `too_large` when the
    /// picture is over [`Settings::gui_max_image`](super::Settings::gui_max_image).
    pub fn screenshot(&self, options: &ScreenshotOptions) -> Result<Screenshot, ToolError> {
        let settings = &self.settings;
        let format = options.format.unwrap_or_default();
        let quality = options.quality.unwrap_or(settings.gui_jpeg_quality);
        let mut wire = Snapshot {
            display: options.display.clone(),
            region: options.region,
            format: Some(match format {
                ShotFormat::Jpeg => ImageFormat::Jpeg,
                _ => ImageFormat::Png,
            }),
            quality: (format == ShotFormat::Jpeg).then_some(quality),
            max_width: Some(options.max_width.unwrap_or(settings.gui_max_width)),
            max_height: Some(options.max_height.unwrap_or(settings.gui_max_height)),
            cursor: true,
            settle: options.settle,
            ..Snapshot::default()
        };
        let (mut gui, mut image) = self.take(&wire)?;
        if format == ShotFormat::Auto && image.bytes.len() as u64 > settings.gui_auto_limit {
            // The screen has just settled: the second picture needs no wait.
            wire.format = Some(ImageFormat::Jpeg);
            wire.quality = Some(quality);
            let settled = image.header.settled;
            (gui, image) = self.take(&Snapshot {
                settle: None,
                ..wire
            })?;
            image.header.settled = image.header.settled.or(settled);
        }
        if image.bytes.len() as u64 > settings.gui_max_image {
            return Err(ToolError::new(
                "too_large",
                format!(
                    "the image is {} bytes, more than the {} a call returns: ask for a smaller `max_width` and `max_height`, a `region`, or `jpeg`",
                    image.bytes.len(),
                    settings.gui_max_image
                ),
            ));
        }
        let header = &image.header;
        let display = Self::display_of(&gui, Some(&header.display))?;
        lock(&self.shots).insert(
            header.display.as_str().to_owned(),
            Shot {
                region: header.region,
                width: header.width,
                height: header.height,
                scale: header.scale,
                display_width: display.width,
                display_height: display.height,
            },
        );
        let cursor = header.cursor.and_then(|point| {
            let shown = |at: i32, origin: u32, length: u32| {
                let offset = i64::from(at) - i64::from(origin);
                (0..i64::from(length))
                    .contains(&offset)
                    .then(|| (offset as f64 * header.scale).round() as i32)
            };
            Some(Point {
                x: shown(point.x, header.region.x, header.region.width)?,
                y: shown(point.y, header.region.y, header.region.height)?,
            })
        });
        Ok(Screenshot {
            mime: mime(header.format),
            display: header.display.clone(),
            width: header.width,
            height: header.height,
            region: header.region,
            scale: header.scale,
            cursor,
            settled: header.settled.map(|settled| settled.settled),
            data: image.bytes,
        })
    }

    /// The displays and what the agent last saw of each.
    ///
    /// # Errors
    ///
    /// The host's, when the stream cannot be opened.
    pub fn gui_displays(&self) -> Result<Displays, ToolError> {
        let gui = self.any_desktop()?;
        let state = gui.state();
        let shots = lock(&self.shots);
        Ok(Displays {
            active: state.active,
            locked: state.locked,
            displays: state
                .displays
                .into_iter()
                .map(|display| DisplayInfo {
                    screenshot: shots
                        .get(display.id.as_str())
                        .map(|shot| (shot.width, shot.height, shot.scale)),
                    display,
                })
                .collect(),
        })
    }

    /// Sends `events` once. A stream lost after the write may have applied them, so they are never sent again.
    fn send(&self, events: Vec<InputEvent>, display: Option<DisplayId>) -> Result<Done, ToolError> {
        let gui = self.desktop(&[Want::Input])?;
        let applied = gui
            .input(&events, Some(self.settings.gui_timeout))
            .inspect_err(|error| {
                if lost(error) {
                    self.forget_desktop();
                }
            })?;
        let mut done = Done {
            applied,
            screenshot: None,
            screenshot_error: None,
        };
        if self.settings.gui_after_action {
            match self.screenshot(&ScreenshotOptions {
                display,
                ..ScreenshotOptions::default()
            }) {
                Ok(shot) => done.screenshot = Some(shot),
                Err(error) => done.screenshot_error = Some(error),
            }
        }
        Ok(done)
    }

    /// Clicks `button` `count` times at `at`.
    ///
    /// # Errors
    ///
    /// `out_of_bounds` (naming the bounds) or `no_screenshot` for a point that is not on the desktop, `denied` without
    /// `gui.input`, `unavailable` while the screen is locked, `bad_request` for a count of zero.
    pub fn click(&self, at: &Pointer, button: Button, count: u32) -> Result<Done, ToolError> {
        if count == 0 {
            return Err(ToolError::bad("`count` is at least 1"));
        }
        let gui = self.desktop(&[Want::Input])?;
        let (display, x, y) = self.resolve(&gui, at)?;
        self.send(
            vec![InputEvent::Button {
                button,
                action: ButtonAction::Click,
                count: Some(count),
                display: Some(display.clone()),
                x: Some(x),
                y: Some(y),
            }],
            Some(display),
        )
    }

    /// Moves the pointer to `at`.
    ///
    /// # Errors
    ///
    /// As [`Tools::click`].
    pub fn move_to(&self, at: &Pointer) -> Result<Done, ToolError> {
        let gui = self.desktop(&[Want::Input])?;
        let (display, x, y) = self.resolve(&gui, at)?;
        self.send(
            vec![InputEvent::Move {
                display: Some(display.clone()),
                x,
                y,
            }],
            Some(display),
        )
    }

    /// Presses `button` at `from`, moves to `to` and lets go. Both points are on one display.
    ///
    /// # Errors
    ///
    /// As [`Tools::click`].
    pub fn drag(&self, from: &Pointer, to: &Pointer, button: Button) -> Result<Done, ToolError> {
        let gui = self.desktop(&[Want::Input])?;
        let (display, fx, fy) = self.resolve(&gui, from)?;
        let mut to = to.clone();
        to.display = Some(display.clone());
        let (_, tx, ty) = self.resolve(&gui, &to)?;
        self.send(
            vec![
                InputEvent::Button {
                    button,
                    action: ButtonAction::Press,
                    count: None,
                    display: Some(display.clone()),
                    x: Some(fx),
                    y: Some(fy),
                },
                InputEvent::Move {
                    display: Some(display.clone()),
                    x: tx,
                    y: ty,
                },
                InputEvent::Button {
                    button,
                    action: ButtonAction::Release,
                    count: None,
                    display: None,
                    x: None,
                    y: None,
                },
            ],
            Some(display),
        )
    }

    /// Turns the wheel `dy` notches down and `dx` to the right (negative the other way), after moving to `at` when given.
    ///
    /// # Errors
    ///
    /// As [`Tools::click`]; `bad_request` when both are zero.
    pub fn scroll(&self, at: Option<&Pointer>, dx: i32, dy: i32) -> Result<Done, ToolError> {
        if dx == 0 && dy == 0 {
            return Err(ToolError::bad("give `dx` or `dy` other than 0"));
        }
        let gui = self.desktop(&[Want::Input])?;
        let (display, x, y) = match at {
            Some(at) => {
                let (display, x, y) = self.resolve(&gui, at)?;
                (Some(display), Some(x), Some(y))
            }
            None => (None, None, None),
        };
        self.send(
            vec![InputEvent::Scroll {
                display: display.clone(),
                x,
                y,
                dx,
                dy,
            }],
            display,
        )
    }

    /// Types `text` as it is, whatever the keyboard layout.
    ///
    /// # Errors
    ///
    /// As [`Tools::click`] without the point; `bad_request` for empty text.
    pub fn type_text(&self, text: &str) -> Result<Done, ToolError> {
        if text.is_empty() {
            return Err(ToolError::bad("`text` is empty"));
        }
        self.send(
            vec![InputEvent::Text {
                text: text.to_owned(),
            }],
            None,
        )
    }

    /// Presses a key combination in xdotool's spelling (`ctrl+s`, `Return`, `super`): the modifiers go down, the last key is
    /// clicked, the modifiers come up in reverse.
    ///
    /// # Errors
    ///
    /// As [`Tools::type_text`]; `bad_request` naming the part that is not a key.
    pub fn key(&self, combo: &str) -> Result<Done, ToolError> {
        let events =
            InputEvent::combo(combo).map_err(|error| ToolError::bad(format!("`keys`: {error}")))?;
        self.send(events, None)
    }

    /// Waits on the host for the screen to stop changing: one round trip, whatever the wait.
    ///
    /// # Errors
    ///
    /// The host's; `denied` without `gui.view`.
    pub fn wait_settled(
        &self,
        display: Option<&DisplayId>,
        options: SettleOptions,
    ) -> Result<Waited, ToolError> {
        let request = SettleRequest {
            display: display.cloned(),
            quiet_ms: options.quiet_ms,
            timeout_ms: options.timeout_ms,
            ..SettleRequest::default()
        };
        let mut tried = 0;
        loop {
            let gui = self.desktop(&[Want::View])?;
            match gui.settle(&request, Some(self.settings.gui_timeout)) {
                Ok(result) => {
                    return Ok(Waited {
                        settled: result.settled,
                        waited_ms: result.waited_ms,
                    });
                }
                Err(error) if lost(&error) && tried == 0 => {
                    tried += 1;
                    self.forget_desktop();
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Where the pointer is. It costs a one-pixel snapshot, which counts against the host's snapshot rate.
    ///
    /// # Errors
    ///
    /// As [`Tools::screenshot`]; `unavailable` when the pointer is on no display.
    pub fn cursor(&self) -> Result<CursorPosition, ToolError> {
        let gui = self.desktop(&[Want::View])?;
        let displays = gui.state().displays;
        for display in displays {
            let (_, image) = self.take(&Snapshot {
                display: Some(display.id.clone()),
                region: Some(Rect::new(0, 0, 1, 1)),
                format: Some(ImageFormat::Png),
                cursor: true,
                ..Snapshot::default()
            })?;
            let Some(point) = image.header.cursor else {
                continue;
            };
            let screenshot = lock(&self.shots)
                .get(display.id.as_str())
                .copied()
                .filter(|shot| {
                    (shot.display_width, shot.display_height) == (display.width, display.height)
                })
                .and_then(|shot| {
                    let shown = |at: i32, origin: u32, length: u32| {
                        let offset = i64::from(at) - i64::from(origin);
                        (0..i64::from(length))
                            .contains(&offset)
                            .then(|| (offset as f64 * shot.scale).round() as i32)
                    };
                    Some(Point {
                        x: shown(point.x, shot.region.x, shot.region.width)?,
                        y: shown(point.y, shot.region.y, shot.region.height)?,
                    })
                });
            return Ok(CursorPosition {
                display: display.id,
                x: point.x,
                y: point.y,
                screenshot,
            });
        }
        Err(ToolError::new(
            "unavailable",
            "the host does not say where the pointer is",
        ))
    }

    /// The desktop's clipboard text.
    ///
    /// # Errors
    ///
    /// `denied` without `gui.clipboard`, and as [`Tools::screenshot`].
    pub fn clipboard_read(&self) -> Result<String, ToolError> {
        let gui = self.clipboard_desktop()?;
        gui.clipboard(Some(self.settings.gui_timeout))
            .map_err(|error| {
                if lost(&error) {
                    self.forget_desktop();
                }
                error.into()
            })
    }

    /// Replaces the desktop's clipboard text.
    ///
    /// # Errors
    ///
    /// As [`Tools::clipboard_read`]. A write that is lost is not sent again.
    pub fn clipboard_write(&self, text: &str) -> Result<(), ToolError> {
        let gui = self.clipboard_desktop()?;
        gui.set_clipboard(text, Some(self.settings.gui_timeout))
            .map_err(|error| {
                if lost(&error) {
                    self.forget_desktop();
                }
                error.into()
            })
    }
}

// ---- the tools -------------------------------------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum SettleArg {
    Flag(bool),
    Options(SettleOptions),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScreenshotArgs {
    display: Option<DisplayId>,
    region: Option<Rect>,
    max_width: Option<u32>,
    max_height: Option<u32>,
    format: Option<ShotFormat>,
    quality: Option<u8>,
    settle: Option<SettleArg>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClickArgs {
    x: f64,
    y: f64,
    button: Option<Button>,
    count: Option<u32>,
    display: Option<DisplayId>,
    space: Option<Space>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MoveArgs {
    x: f64,
    y: f64,
    display: Option<DisplayId>,
    space: Option<Space>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Xy {
    x: f64,
    y: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DragArgs {
    from: Xy,
    to: Xy,
    button: Option<Button>,
    display: Option<DisplayId>,
    space: Option<Space>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScrollArgs {
    x: Option<f64>,
    y: Option<f64>,
    dx: Option<i32>,
    dy: Option<i32>,
    display: Option<DisplayId>,
    space: Option<Space>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextArgs {
    text: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct KeyArgs {
    keys: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitArgs {
    quiet_ms: Option<u64>,
    timeout_ms: Option<u64>,
    display: Option<DisplayId>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchArgs {
    duration_ms: Option<u64>,
    display: Option<DisplayId>,
    region: Option<Rect>,
    fps: Option<u32>,
    max_width: Option<u32>,
    max_height: Option<u32>,
    #[serde(default)]
    cursor: bool,
}

fn summary(shot: &Screenshot) -> Value {
    json!({
        "display": shot.display,
        "width": shot.width,
        "height": shot.height,
        "scale": shot.scale,
        "region": shot.region,
        "cursor": shot.cursor,
        "settled": shot.settled,
        "bytes": shot.data.len(),
    })
}

fn picture(shot: Screenshot) -> Output {
    Output {
        value: summary(&shot),
        images: vec![Image {
            mime: shot.mime.to_owned(),
            data: shot.data,
        }],
    }
}

/// An input's answer, with the settled screenshot that follows it when the tools take one.
fn acted(done: Done) -> Output {
    let mut value = json!({"applied": done.applied});
    let mut images = Vec::new();
    if let Some(shot) = done.screenshot {
        value["screenshot"] = summary(&shot);
        images.push(Image {
            mime: shot.mime.to_owned(),
            data: shot.data,
        });
    }
    if let Some(error) = done.screenshot_error {
        value["screenshot_error"] = json!({"code": error.code, "message": error.message});
    }
    Output { value, images }
}

fn pointer(x: f64, y: f64, display: Option<DisplayId>, space: Option<Space>) -> Pointer {
    Pointer {
        x,
        y,
        display,
        space: space.unwrap_or_default(),
    }
}

pub(super) fn screenshot(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: ScreenshotArgs = parse(arguments)?;
    let options = ScreenshotOptions {
        display: args.display,
        region: args.region,
        max_width: args.max_width,
        max_height: args.max_height,
        format: args.format,
        quality: args.quality,
        settle: match args.settle {
            Some(SettleArg::Options(options)) => Some(options),
            Some(SettleArg::Flag(false)) => None,
            Some(SettleArg::Flag(true)) | None => Some(SettleOptions::default()),
        },
    };
    tools.screenshot(&options).map(picture)
}

pub(super) fn click(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: ClickArgs = parse(arguments)?;
    tools
        .click(
            &pointer(args.x, args.y, args.display, args.space),
            args.button.unwrap_or(Button::Left),
            args.count.unwrap_or(1),
        )
        .map(acted)
}

pub(super) fn move_pointer(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: MoveArgs = parse(arguments)?;
    tools
        .move_to(&pointer(args.x, args.y, args.display, args.space))
        .map(acted)
}

pub(super) fn drag(tools: &Tools, arguments: Value, _cancel: &Cancel) -> Result<Output, ToolError> {
    let args: DragArgs = parse(arguments)?;
    tools
        .drag(
            &pointer(args.from.x, args.from.y, args.display.clone(), args.space),
            &pointer(args.to.x, args.to.y, args.display, args.space),
            args.button.unwrap_or(Button::Left),
        )
        .map(acted)
}

pub(super) fn scroll(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: ScrollArgs = parse(arguments)?;
    let at = match (args.x, args.y) {
        (Some(x), Some(y)) => Some(pointer(x, y, args.display, args.space)),
        (None, None) => None,
        _ => return Err(ToolError::bad("give both `x` and `y`, or neither")),
    };
    tools
        .scroll(at.as_ref(), args.dx.unwrap_or(0), args.dy.unwrap_or(0))
        .map(acted)
}

pub(super) fn type_text(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: TextArgs = parse(arguments)?;
    tools.type_text(&args.text).map(acted)
}

pub(super) fn key(tools: &Tools, arguments: Value, _cancel: &Cancel) -> Result<Output, ToolError> {
    let args: KeyArgs = parse(arguments)?;
    tools.key(&args.keys).map(acted)
}

pub(super) fn wait(tools: &Tools, arguments: Value, _cancel: &Cancel) -> Result<Output, ToolError> {
    let args: WaitArgs = parse(arguments)?;
    let waited = tools.wait_settled(
        args.display.as_ref(),
        SettleOptions {
            quiet_ms: args.quiet_ms,
            timeout_ms: args.timeout_ms,
        },
    )?;
    Output::json(&json!({"settled": waited.settled, "waited_ms": waited.waited_ms}))
}

pub(super) fn displays(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let EmptyArgs {} = parse(arguments)?;
    let state = tools.gui_displays()?;
    let listed: Vec<Value> = state
        .displays
        .iter()
        .map(|info| {
            let mut value = serde_json::to_value(&info.display).unwrap_or(Value::Null);
            if let Some((width, height, scale)) = info.screenshot {
                value["last_screenshot"] =
                    json!({"width": width, "height": height, "scale": scale});
            }
            value
        })
        .collect();
    Output::json(&json!({"active": state.active, "locked": state.locked, "displays": listed}))
}

pub(super) fn cursor_position(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let EmptyArgs {} = parse(arguments)?;
    let at = tools.cursor()?;
    Output::json(&json!({
        "display": at.display,
        "x": at.x,
        "y": at.y,
        "screenshot": at.screenshot,
    }))
}

pub(super) fn clipboard_read(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let EmptyArgs {} = parse(arguments)?;
    let text = tools.clipboard_read()?;
    Output::json(&json!({"text": text}))
}

pub(super) fn clipboard_write(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: TextArgs = parse(arguments)?;
    tools.clipboard_write(&args.text)?;
    Output::json(&json!({"written": args.text.len()}))
}

pub(super) fn watch(tools: &Tools, arguments: Value, cancel: &Cancel) -> Result<Output, ToolError> {
    let args: WatchArgs = parse(arguments)?;
    let length =
        Duration::from_millis(args.duration_ms.unwrap_or(2000)).min(tools.settings.gui_timeout);
    let gui = tools.desktop(&[Want::View])?;
    gui.watch(&Watch {
        display: args.display,
        region: args.region,
        fps: args.fps,
        max_width: args.max_width,
        max_height: args.max_height,
        cursor: args.cursor,
        ..Watch::default()
    })?;
    let begun = Instant::now();
    let (mut updates, mut rects, mut bytes, mut cursors) = (0_u64, 0_u64, 0_u64, 0_u64);
    let mut size = None;
    let result = loop {
        if cancel.is_cancelled() || begun.elapsed() >= length {
            break Ok(());
        }
        match gui.next_event(Some(tools.settings.poll)) {
            Ok(WatchEvent::Update(update)) => {
                updates += 1;
                rects += update.header.rects.len() as u64;
                bytes += update.bytes;
                size = Some((update.header.width, update.header.height));
            }
            Ok(WatchEvent::Cursor(_)) => cursors += 1,
            Err(Error::Timeout) => {}
            Err(error) => break Err(error),
        }
    };
    let _ = gui.unwatch();
    if let Err(error) = result {
        if lost(&error) {
            tools.forget_desktop();
        }
        return Err(error.into());
    }
    Output::json(&json!({
        "updates": updates,
        "rectangles": rects,
        "update_bytes": bytes,
        "cursor_moves": cursors,
        "width": size.map(|(width, _)| width),
        "height": size.map(|(_, height)| height),
        "duration_ms": u64::try_from(begun.elapsed().as_millis()).unwrap_or(u64::MAX),
    }))
}

/// The screenshots kept, per display.
pub(crate) type Shots = HashMap<String, Shot>;
