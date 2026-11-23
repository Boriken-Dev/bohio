//! A desktop that exists only in memory: two displays of painted pixels, input that paints and records, a clipboard.
//! The tests and the `gui_host` example include it with `#[path]`; nothing here touches a screen.
#![allow(dead_code, reason = "each includer uses some of it")]

use std::sync::Mutex;

use bohio::gui::{Capabilities, Desktop, DesktopState, Frame, GuiError};
use bohio_proto::gui::{Button, ButtonAction, Display, DisplayId, InputEvent, KeyAction, Rect};

/// The side of the square a click paints.
pub const MARK: u32 = 5;
/// The colour of the square a click paints.
pub const CLICK: [u8; 3] = [255, 0, 0];

struct Screen {
    id: DisplayId,
    width: u32,
    height: u32,
    rgb: Vec<u8>,
}

struct Inner {
    screens: Vec<Screen>,
    pointer: (usize, i32, i32),
    active: bool,
    locked: Option<bool>,
    log: Vec<String>,
    clipboard: String,
    calls: u32,
}

/// The painted desktop. Display `0` is 320 by 200 and primary; display `1` is 160 by 100.
pub struct Painted {
    inner: Mutex<Inner>,
}

/// The colour a pixel of a display starts with: a pattern a snapshot must reproduce exactly.
pub fn pattern(display: u32, x: u32, y: u32) -> [u8; 3] {
    [
        (x * 7 + display * 50) as u8,
        (y * 11) as u8,
        ((x ^ y) * 3) as u8,
    ]
}

impl Painted {
    pub fn new() -> Self {
        let screens = [(0_u32, 320_u32, 200_u32), (1, 160, 100)]
            .into_iter()
            .map(|(index, width, height)| {
                let mut rgb = Vec::with_capacity((width * height * 3) as usize);
                for y in 0..height {
                    for x in 0..width {
                        rgb.extend_from_slice(&pattern(index, x, y));
                    }
                }
                Screen {
                    id: DisplayId(index.to_string()),
                    width,
                    height,
                    rgb,
                }
            })
            .collect();
        Self {
            inner: Mutex::new(Inner {
                screens,
                pointer: (0, 0, 0),
                active: true,
                locked: Some(false),
                log: Vec::new(),
                clipboard: String::new(),
                calls: 0,
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn set_locked(&self, locked: Option<bool>) {
        self.lock().locked = locked;
    }

    pub fn set_active(&self, active: bool) {
        self.lock().active = active;
    }

    /// Fills a rectangle of a display.
    pub fn paint(&self, display: usize, rect: Rect, color: [u8; 3]) {
        paint(&mut self.lock().screens[display], rect, color);
    }

    /// Fills a rectangle of a display with `color(x, y)` for each of its pixels.
    pub fn paint_with(&self, display: usize, rect: Rect, color: impl Fn(u32, u32) -> [u8; 3]) {
        let mut inner = self.lock();
        let screen = &mut inner.screens[display];
        for y in rect.y..(rect.y + rect.height).min(screen.height) {
            for x in rect.x..(rect.x + rect.width).min(screen.width) {
                let at = ((y * screen.width + x) * 3) as usize;
                screen.rgb[at..at + 3].copy_from_slice(&color(x, y));
            }
        }
    }

    /// Every input event the desktop took, as text: `press:leftx1@0:5,6`, `key:enter:click`, `text:abc`.
    pub fn log(&self) -> Vec<String> {
        self.lock().log.clone()
    }

    pub fn clipboard(&self) -> String {
        self.lock().clipboard.clone()
    }

    /// How many calls reached the desktop (state, capture, input, clipboard).
    pub fn calls(&self) -> u32 {
        self.lock().calls
    }

    /// The width, height and pixels of a display.
    pub fn pixels(&self, display: usize) -> (u32, u32, Vec<u8>) {
        let inner = self.lock();
        let screen = &inner.screens[display];
        (screen.width, screen.height, screen.rgb.clone())
    }
}

fn paint(screen: &mut Screen, rect: Rect, color: [u8; 3]) {
    for y in rect.y..(rect.y + rect.height).min(screen.height) {
        for x in rect.x..(rect.x + rect.width).min(screen.width) {
            let at = ((y * screen.width + x) * 3) as usize;
            screen.rgb[at..at + 3].copy_from_slice(&color);
        }
    }
}

impl Desktop for Painted {
    fn state(&self) -> Result<DesktopState, GuiError> {
        let mut inner = self.lock();
        inner.calls += 1;
        let displays = inner
            .screens
            .iter()
            .enumerate()
            .map(|(index, screen)| Display {
                id: screen.id.clone(),
                name: Some(format!("painted {index}")),
                primary: index == 0,
                x: if index == 0 { 0 } else { 320 },
                y: 0,
                width: screen.width,
                height: screen.height,
                scale: 1.0,
                rotation: Some(0),
            })
            .collect();
        Ok(DesktopState {
            user: Some("painter".to_owned()),
            active: inner.active,
            locked: inner.locked,
            displays,
        })
    }

    fn capture(&self, display: &DisplayId, region: Option<Rect>) -> Result<Frame, GuiError> {
        let mut inner = self.lock();
        inner.calls += 1;
        let screen = inner
            .screens
            .iter()
            .find(|screen| &screen.id == display)
            .ok_or_else(|| GuiError::not_found("no such display"))?;
        let rect = region.unwrap_or(Rect::new(0, 0, screen.width, screen.height));
        let mut rgb = Vec::with_capacity((rect.width * rect.height * 3) as usize);
        for y in rect.y..rect.y + rect.height {
            let from = ((y * screen.width + rect.x) * 3) as usize;
            rgb.extend_from_slice(&screen.rgb[from..from + (rect.width * 3) as usize]);
        }
        Frame::new(rect.width, rect.height, rgb)
    }

    fn input(&self, events: &[InputEvent]) -> Result<u32, GuiError> {
        let mut inner = self.lock();
        inner.calls += 1;
        for event in events {
            let index_of = |inner: &Inner, display: &Option<DisplayId>| {
                display.as_ref().map_or(0, |id| {
                    inner.screens.iter().position(|s| &s.id == id).unwrap_or(0)
                })
            };
            match event {
                InputEvent::Move { display, x, y } => {
                    let index = index_of(&inner, display);
                    inner.pointer = (index, *x, *y);
                    inner.log.push(format!("move:{index}@{x},{y}"));
                }
                InputEvent::Button {
                    button,
                    action,
                    count,
                    display,
                    x,
                    y,
                } => {
                    let index = index_of(&inner, display);
                    if let (Some(x), Some(y)) = (x, y) {
                        inner.pointer = (index, *x, *y);
                    }
                    let (at, px, py) = inner.pointer;
                    if *button == Button::Left && *action != ButtonAction::Release {
                        let rect = Rect::new(px.max(0) as u32, py.max(0) as u32, MARK, MARK);
                        paint(&mut inner.screens[at], rect, CLICK);
                    }
                    let name = format!("{button:?}").to_lowercase();
                    let action = format!("{action:?}").to_lowercase();
                    inner.log.push(format!(
                        "{action}:{name}x{}@{at}:{px},{py}",
                        count.unwrap_or(1)
                    ));
                }
                InputEvent::Scroll { dx, dy, .. } => inner.log.push(format!("scroll:{dx},{dy}")),
                InputEvent::Key { key, action } => {
                    let action = match action {
                        KeyAction::Press => "press",
                        KeyAction::Release => "release",
                        KeyAction::Click => "click",
                    };
                    inner.log.push(format!("key:{key}:{action}"));
                }
                InputEvent::Text { text } => inner.log.push(format!("text:{text}")),
                InputEvent::Wait { .. } => {}
            }
        }
        Ok(u32::try_from(events.len()).unwrap_or(u32::MAX))
    }

    fn cursor(&self) -> Option<(DisplayId, i32, i32)> {
        let inner = self.lock();
        let (index, x, y) = inner.pointer;
        Some((inner.screens[index].id.clone(), x, y))
    }

    fn clipboard_text(&self) -> Result<String, GuiError> {
        let mut inner = self.lock();
        inner.calls += 1;
        Ok(inner.clipboard.clone())
    }

    fn set_clipboard_text(&self, text: &str) -> Result<(), GuiError> {
        let mut inner = self.lock();
        inner.calls += 1;
        inner.clipboard = text.to_owned();
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            backend: "painted".to_owned(),
            input: true,
            clipboard: true,
            lock_detection: true,
        }
    }
}
