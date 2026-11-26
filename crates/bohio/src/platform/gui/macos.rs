//! The macOS desktop: the screen of the console session the node runs in, as a [`Desktop`].
//!
//! Capture is ScreenCaptureKit's `SCScreenshotManager` (macOS 14 and later), asked for one display at its backing pixels
//! and cut to the region in Rust. The displays and the login session's state come from Core Graphics, which needs no
//! permission. Input is `enigo` (CGEvents posted to the HID tap). The clipboard is the general pasteboard through
//! `arboard`. Screen Recording and Accessibility are granted to a program by a person in System Settings; the node asks
//! without prompting (`CGPreflightScreenCaptureAccess`, enigo's trust check) and reports a missing one as `unavailable`
//! with the pane and the program's path.
//!
//! This module is one of the files that hold `unsafe`: the Objective-C methods of ScreenCaptureKit and the display list
//! are unsafe functions in their bindings, and no safe binding builds on Xcode 16 (see `tests/unsafe_files.rs`).

#![allow(unsafe_code)]

use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use block2::RcBlock;
use bohio_proto::ErrorCode;
use bohio_proto::gui::{Button, DisplayId, InputEvent, KeyAction, Rect};
use enigo::{
    Axis, Coordinate, Direction, Enigo, InputError, Keyboard as _, Mouse as _, NewConError,
    Settings,
};
use objc2::AnyThread as _;
use objc2::rc::Retained;
use objc2::runtime::AnyClass;
use objc2_core_foundation::{CFBoolean, CFString, CFType};
use objc2_core_graphics::{
    CGDataProvider, CGDisplayBounds, CGDisplayCopyDisplayMode, CGDisplayIsMain, CGDisplayMode,
    CGDisplayRotation, CGEvent, CGGetActiveDisplayList, CGImage, CGPreflightScreenCaptureAccess,
    CGSessionCopyCurrentDictionary,
};
use objc2_foundation::{NSArray, NSError};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCScreenshotManager, SCShareableContent, SCStreamConfiguration,
    SCWindow,
};

use super::mac_rules::{
    Key, Named, RawDisplay, SessionFacts, Step, accessibility_denied, displays_of, judge,
    packed_of, pixel_of, plan, readable_pixels, rgb_of_packed, screen_recording_denied,
};
use crate::gui::{Capabilities, Desktop, DesktopState, Frame, GuiError};

/// What the user declined, as ScreenCaptureKit numbers it (`SCStreamErrorUserDeclined`).
const USER_DECLINED: isize = -3801;

/// How the macOS desktop behaves.
#[derive(Debug, Clone)]
pub struct MacosSettings {
    /// How long a capture waits for ScreenCaptureKit's answer before it gives up.
    pub capture_wait: Duration,
    /// The most bytes of clipboard text read or written.
    pub clipboard_max_bytes: usize,
    /// The most clicks of one button event and wheel notches of one scroll event; more is `bad_request`.
    pub max_repeat: u32,
}

impl Default for MacosSettings {
    fn default() -> Self {
        Self {
            capture_wait: Duration::from_secs(10),
            clipboard_max_bytes: 1024 * 1024,
            max_repeat: 1_000,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The program's own path, as the grants of System Settings name it.
fn binary() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("this program"))
}

// ----------------------------------------------------------------------------------------------------------------
// The displays and the session.

/// The displays that are on, as the window server lays them out.
fn raw_displays() -> Vec<RawDisplay> {
    let mut count = 0_u32;
    // SAFETY: a null buffer with a zero capacity only asks how many displays there are; `count` is a valid pointer.
    let asked = unsafe { CGGetActiveDisplayList(0, ptr::null_mut(), &raw mut count) };
    if asked.0 != 0 || count == 0 {
        return Vec::new();
    }
    let mut ids = vec![0_u32; count as usize];
    // SAFETY: `ids` holds `count` elements and `count` is a valid pointer.
    let listed = unsafe { CGGetActiveDisplayList(count, ids.as_mut_ptr(), &raw mut count) };
    if listed.0 != 0 {
        return Vec::new();
    }
    ids.truncate(count as usize);
    ids.into_iter()
        .map(|id| {
            let bounds = CGDisplayBounds(id);
            let (pixel_width, pixel_height) = CGDisplayCopyDisplayMode(id).map_or((0, 0), |mode| {
                (
                    u32::try_from(CGDisplayMode::pixel_width(Some(&mode))).unwrap_or(0),
                    u32::try_from(CGDisplayMode::pixel_height(Some(&mode))).unwrap_or(0),
                )
            });
            RawDisplay {
                id,
                main: CGDisplayIsMain(id),
                x: bounds.origin.x,
                y: bounds.origin.y,
                width: bounds.size.width,
                height: bounds.size.height,
                pixel_width,
                pixel_height,
                rotation: CGDisplayRotation(id),
            }
        })
        .collect()
}

/// A boolean the session dictionary holds under `name`.
fn session_flag(dictionary: &objc2_core_foundation::CFDictionary, name: &str) -> Option<bool> {
    let key = CFString::from_str(name);
    let mut found: *const c_void = ptr::null();
    // SAFETY: `key` is a live CFString for the call and `found` a valid out-pointer; the dictionary is not modified.
    let present = unsafe {
        dictionary.value_if_present(ptr::from_ref::<CFString>(&key).cast(), &raw mut found)
    };
    if !present || found.is_null() {
        return None;
    }
    // SAFETY: the dictionary owns the value, which lives as long as `dictionary` does; it is only read as a CFType.
    let value: &CFType = unsafe { &*found.cast::<CFType>() };
    value.downcast_ref::<CFBoolean>().map(CFBoolean::value)
}

/// What the window server says of the login session, or `None` when this process has none.
fn session_facts() -> Option<SessionFacts> {
    let dictionary = CGSessionCopyCurrentDictionary()?;
    Some(SessionFacts {
        on_console: session_flag(&dictionary, "kCGSSessionOnConsoleKey"),
        screen_locked: session_flag(&dictionary, "CGSSessionScreenIsLocked"),
    })
}

// ----------------------------------------------------------------------------------------------------------------
// Capture.

/// One display's pixels, or why there are none.
type Grabbed = Result<Vec<u8>, GuiError>;

/// What a capture is of: a display, its backing size, and the part of it wanted (left, top, width, height in pixels).
#[derive(Clone, Copy)]
struct Target {
    display: u32,
    width: usize,
    height: usize,
    region: (usize, usize, usize, usize),
}

fn description(error: *mut NSError) -> String {
    // SAFETY: the system passes a valid NSError or null; it is only read here.
    unsafe { error.as_ref() }.map_or_else(
        || "no reason was given".to_owned(),
        |error| error.localizedDescription().to_string(),
    )
}

fn declined(error: *mut NSError) -> bool {
    // SAFETY: as in `description`.
    unsafe { error.as_ref() }.is_some_and(|error| error.code() == USER_DECLINED)
}

fn failure(error: *mut NSError) -> GuiError {
    if declined(error) {
        GuiError::unavailable(screen_recording_denied(&binary()))
    } else {
        GuiError::unavailable(format!(
            "the screen could not be captured: {}",
            description(error)
        ))
    }
}

/// The picture ScreenCaptureKit handed over, as the RGB bytes of the wanted region.
fn convert(image: *mut CGImage, target: Target) -> Grabbed {
    // SAFETY: the system passes a valid CGImage or null; it is only read while the handler runs.
    let image = unsafe { image.as_ref() }
        .ok_or_else(|| GuiError::unavailable("the system returned no picture of the screen"))?;
    let (width, height) = (CGImage::width(Some(image)), CGImage::height(Some(image)));
    if (width, height) != (target.width, target.height) {
        return Err(GuiError::io(format!(
            "the screenshot is {width} by {height} pixels, not the display's {} by {}",
            target.width, target.height
        )));
    }
    let packed = packed_of(
        CGImage::bits_per_component(Some(image)),
        CGImage::bits_per_pixel(Some(image)),
        CGImage::bitmap_info(Some(image)).0,
    )?;
    let provider = CGImage::data_provider(Some(image))
        .ok_or_else(|| GuiError::io("the screenshot has no pixel data"))?;
    let data = CGDataProvider::data(Some(&provider))
        .ok_or_else(|| GuiError::io("the screenshot's pixels could not be read"))?;
    let pointer = data.byte_ptr();
    let length = readable_pixels(pointer, usize::try_from(data.length()).unwrap_or(0))?;
    // SAFETY: `pointer` is not null and the CFData holds `length` bytes there for as long as `data` is alive.
    let bytes = unsafe { std::slice::from_raw_parts(pointer, length) };
    rgb_of_packed(
        bytes,
        CGImage::bytes_per_row(Some(image)),
        width,
        height,
        packed,
        target.region,
    )
}

/// Takes the screenshot of the display the content lists, and sends the region's pixels.
fn on_content(
    content: *mut SCShareableContent,
    error: *mut NSError,
    target: Target,
    out: mpsc::Sender<Grabbed>,
) {
    // SAFETY: the system passes a valid SCShareableContent or null; it is only read while the handler runs.
    let Some(content) = (unsafe { content.as_ref() }) else {
        let _ = out.send(Err(failure(error)));
        return;
    };
    // SAFETY: plain property reads of objects the system made.
    let displays = unsafe { content.displays() };
    let mut found: Option<Retained<SCDisplay>> = None;
    for index in 0..displays.count() {
        let candidate = displays.objectAtIndex(index);
        // SAFETY: as above.
        if unsafe { candidate.displayID() } == target.display {
            found = Some(candidate);
            break;
        }
    }
    let Some(display) = found else {
        let _ = out.send(Err(GuiError::not_found("the display is gone")));
        return;
    };
    let nothing: Retained<NSArray<SCWindow>> = NSArray::new();
    // SAFETY: the filter is made from a display ScreenCaptureKit listed and an empty list of windows to leave out; the
    // configuration is set to the display's backing size and keeps the pointer out of the picture.
    let (filter, configuration) = unsafe {
        let filter = SCContentFilter::initWithDisplay_excludingWindows(
            SCContentFilter::alloc(),
            &display,
            &nothing,
        );
        let configuration = SCStreamConfiguration::new();
        configuration.setWidth(target.width);
        configuration.setHeight(target.height);
        configuration.setShowsCursor(false);
        (filter, configuration)
    };
    let done = {
        let out = out.clone();
        RcBlock::new(move |image: *mut CGImage, error: *mut NSError| {
            let picture = if image.is_null() {
                Err(failure(error))
            } else {
                convert(image, target)
            };
            let _ = out.send(picture);
        })
    };
    // SAFETY: the filter, the configuration and the block are valid for the call; the system copies the block.
    unsafe {
        SCScreenshotManager::captureImageWithFilter_configuration_completionHandler(
            &filter,
            &configuration,
            Some(&done),
        );
    }
}

/// The RGB bytes of the target, or why they could not be had.
fn grab(target: Target, wait: Duration) -> Grabbed {
    if AnyClass::get(c"SCScreenshotManager").is_none() {
        return Err(GuiError::unavailable(
            "capturing the screen needs macOS 14 or later",
        ));
    }
    let (sender, receiver) = mpsc::channel::<Grabbed>();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            on_content(content, error, target, sender.clone());
        },
    );
    // SAFETY: the block is valid for the call and the system copies it.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    match receiver.recv_timeout(wait) {
        Ok(picture) => picture,
        Err(RecvTimeoutError::Timeout) => Err(GuiError::unavailable(
            "the screen capture did not answer in time",
        )),
        Err(RecvTimeoutError::Disconnected) => {
            Err(GuiError::io("the screen capture ended without a picture"))
        }
    }
}

// ----------------------------------------------------------------------------------------------------------------
// Input.

fn input_failed(error: &InputError) -> GuiError {
    // The error's text can name the key: nothing typed is shown to a peer.
    let _ = error;
    GuiError::io("the event system refused the input")
}

fn named_key(named: Named) -> enigo::Key {
    use enigo::Key as K;
    match named {
        Named::Enter => K::Return,
        Named::Tab => K::Tab,
        Named::Escape => K::Escape,
        Named::Backspace => K::Backspace,
        Named::Delete => K::Delete,
        Named::Help => K::Help,
        Named::Home => K::Home,
        Named::End => K::End,
        Named::PageUp => K::PageUp,
        Named::PageDown => K::PageDown,
        Named::Up => K::UpArrow,
        Named::Down => K::DownArrow,
        Named::Left => K::LeftArrow,
        Named::Right => K::RightArrow,
        Named::Space => K::Space,
        Named::Shift => K::Shift,
        Named::Control => K::Control,
        Named::Alt => K::Option,
        Named::Meta => K::Meta,
        Named::CapsLock => K::CapsLock,
        Named::Function(number) => match number {
            1 => K::F1,
            2 => K::F2,
            3 => K::F3,
            4 => K::F4,
            5 => K::F5,
            6 => K::F6,
            7 => K::F7,
            8 => K::F8,
            9 => K::F9,
            10 => K::F10,
            11 => K::F11,
            12 => K::F12,
            13 => K::F13,
            14 => K::F14,
            15 => K::F15,
            16 => K::F16,
            17 => K::F17,
            18 => K::F18,
            19 => K::F19,
            _ => K::F20,
        },
    }
}

fn direction(action: KeyAction) -> Direction {
    match action {
        KeyAction::Press => Direction::Press,
        KeyAction::Release => Direction::Release,
        KeyAction::Click => Direction::Click,
    }
}

fn button_of(button: Button) -> enigo::Button {
    match button {
        Button::Left => enigo::Button::Left,
        Button::Right => enigo::Button::Right,
        Button::Middle => enigo::Button::Middle,
        Button::Back => enigo::Button::Back,
        Button::Forward => enigo::Button::Forward,
    }
}

/// Posts one step.
fn send(enigo: &mut Enigo, step: &Step) -> Result<(), GuiError> {
    let sent = match step {
        Step::Move { x, y } => enigo.move_mouse(*x, *y, Coordinate::Abs),
        Step::Button { button, down } => enigo.button(
            button_of(*button),
            if *down {
                Direction::Press
            } else {
                Direction::Release
            },
        ),
        Step::Scroll {
            notches,
            horizontal,
        } => enigo.scroll(
            *notches,
            if *horizontal {
                Axis::Horizontal
            } else {
                Axis::Vertical
            },
        ),
        Step::Key { key, action } => {
            let key = match key {
                Key::Named(named) => named_key(*named),
                Key::Char(character) => enigo::Key::Unicode(*character),
            };
            enigo.key(key, direction(*action))
        }
        Step::Text(text) => enigo.text(text),
    };
    sent.map_err(|error| input_failed(&error))
}

// ----------------------------------------------------------------------------------------------------------------
// The desktop.

/// The screen of the console session, served as a [`Desktop`].
///
/// Nothing is asked of the system until a request comes: a node that serves no GUI never touches it.
pub struct MacosDesktop {
    settings: MacosSettings,
    /// The event source: made at the first input, kept so that a key held by one request is still held for the next.
    enigo: Mutex<Option<Enigo>>,
}

impl MacosDesktop {
    /// A desktop run by `settings`.
    #[must_use]
    pub fn new(settings: MacosSettings) -> Self {
        Self {
            settings,
            enigo: Mutex::new(None),
        }
    }

    /// Runs `f` on the event source, made first if there is none.
    fn with_enigo<R>(
        &self,
        f: impl FnOnce(&mut Enigo) -> Result<R, GuiError>,
    ) -> Result<R, GuiError> {
        let mut held = lock(&self.enigo);
        if held.is_none() {
            let made = Enigo::new(&Settings::default()).map_err(|error| match error {
                NewConError::NoPermission => GuiError::unavailable(accessibility_denied(&binary())),
                _ => GuiError::unavailable("the system's event source could not be made"),
            })?;
            *held = Some(made);
        }
        match held.as_mut() {
            Some(enigo) => f(enigo),
            None => Err(GuiError::unavailable("the event source is not made")),
        }
    }
}

impl Desktop for MacosDesktop {
    fn state(&self) -> Result<DesktopState, GuiError> {
        let judged = judge(session_facts())?;
        let displays = displays_of(&raw_displays());
        if displays.is_empty() {
            return Err(GuiError::unavailable("no display is on"));
        }
        Ok(DesktopState {
            user: None,
            active: judged.active,
            locked: Some(judged.locked),
            displays,
        })
    }

    fn capture(&self, display: &DisplayId, region: Option<Rect>) -> Result<Frame, GuiError> {
        // Asked first and without a prompt: asking ScreenCaptureKit without the grant would show the person a dialog, and
        // the grant is what a node started where no one can click needs told before anything else.
        if !CGPreflightScreenCaptureAccess() {
            return Err(GuiError::unavailable(screen_recording_denied(&binary())));
        }
        let state = self.state()?;
        let found = state
            .displays
            .iter()
            .find(|candidate| &candidate.id == display)
            .ok_or_else(|| GuiError::not_found("no such display"))?;
        let region = region.unwrap_or_else(|| Rect::new(0, 0, found.width, found.height));
        let inside = u64::from(region.x) + u64::from(region.width) <= u64::from(found.width)
            && u64::from(region.y) + u64::from(region.height) <= u64::from(found.height);
        if !inside || region.width == 0 || region.height == 0 {
            return Err(GuiError::bad_request("the region lies outside the display"));
        }
        let id = found
            .id
            .as_str()
            .strip_prefix("display")
            .and_then(|digits| digits.parse::<u32>().ok())
            .ok_or_else(|| GuiError::not_found("no such display"))?;
        let rgb = grab(
            Target {
                display: id,
                width: found.width as usize,
                height: found.height as usize,
                region: (
                    region.x as usize,
                    region.y as usize,
                    region.width as usize,
                    region.height as usize,
                ),
            },
            self.settings.capture_wait,
        )?;
        Frame::new(region.width, region.height, rgb)
    }

    fn input(&self, events: &[InputEvent]) -> Result<u32, GuiError> {
        // The grant first, for the reason `capture` asks first.
        self.with_enigo(|_| Ok(()))?;
        let state = self.state()?;
        // Every event is planned before any is sent, so one that names a point off its display refuses the batch whole.
        let planned: Vec<Vec<Step>> = events
            .iter()
            .map(|event| plan(event, &state, self.settings.max_repeat))
            .collect::<Result<_, _>>()?;
        self.with_enigo(|enigo| {
            let mut applied = 0_u32;
            for steps in planned {
                for step in &steps {
                    if let Err(error) = send(enigo, step) {
                        return if applied == 0 {
                            Err(error)
                        } else {
                            Ok(applied)
                        };
                    }
                }
                applied += 1;
            }
            Ok(applied)
        })
    }

    fn cursor(&self) -> Option<(DisplayId, i32, i32)> {
        let place = CGEvent::location(CGEvent::new(None).as_deref());
        let displays = displays_of(&raw_displays());
        #[allow(
            clippy::cast_possible_truncation,
            reason = "a pointer's place is a few thousand points"
        )]
        pixel_of(&displays, place.x.floor() as i32, place.y.floor() as i32)
    }

    fn clipboard_text(&self) -> Result<String, GuiError> {
        let mut clipboard = arboard::Clipboard::new().map_err(clipboard_failed)?;
        let text = clipboard.get_text().map_err(clipboard_failed)?;
        if text.len() > self.settings.clipboard_max_bytes {
            return Err(GuiError::bad_request(
                "the clipboard text is longer than this host reads",
            ));
        }
        Ok(text)
    }

    fn set_clipboard_text(&self, text: &str) -> Result<(), GuiError> {
        if text.len() > self.settings.clipboard_max_bytes {
            return Err(GuiError::bad_request(
                "the clipboard text is longer than this host writes",
            ));
        }
        let mut clipboard = arboard::Clipboard::new().map_err(clipboard_failed)?;
        clipboard.set_text(text).map_err(clipboard_failed)
    }

    fn capabilities(&self) -> Capabilities {
        let made = lock(&self.enigo).is_some();
        let input = made || self.with_enigo(|_| Ok(())).is_ok();
        Capabilities {
            backend: "macos".to_owned(),
            input,
            clipboard: true,
            lock_detection: true,
        }
    }
}

fn clipboard_failed(error: arboard::Error) -> GuiError {
    match error {
        arboard::Error::ContentNotAvailable => GuiError::unavailable("the clipboard holds no text"),
        arboard::Error::ClipboardNotSupported => GuiError::unavailable(
            "the pasteboard cannot be reached: no one is logged in at the screen",
        ),
        _ => GuiError::new(ErrorCode::Io, "the clipboard could not be used"),
    }
}
