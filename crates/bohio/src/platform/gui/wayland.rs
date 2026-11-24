//! The Linux desktop over Wayland: the screen of a session reached through the desktop portals, as a [`Desktop`].
//!
//! A Wayland client cannot read the screen or send input to other programs, so everything goes through
//! `xdg-desktop-portal`: one RemoteDesktop session with ScreenCast monitor sources (input and pictures together), or a
//! ScreenCast session alone where the portal has no RemoteDesktop (`xdg-desktop-portal-wlr`), which is a view and no
//! input. The portal's own dialog is the operating system's approval; it appears when the first request reaches the
//! node (`info` included), is asked again only after `retry`, and its answer is never remembered unless
//! `wayland_persist` says so. The pictures are PipeWire streams, one for each monitor, read on a thread of their own that
//! keeps the latest frame of each. The portals speak D-Bus on `async-io` and are driven with `pollster` (no Tokio).
//!
//! The rules that need no compositor (pixel layouts, the layout of the monitors, the input plan) are plain functions
//! tested without one.

#![forbid(unsafe_code)]

use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use ashpd::desktop::PersistMode;
use ashpd::desktop::remote_desktop::{
    Axis, DeviceType, KeyState, RemoteDesktop, SelectDevicesOptions,
};
use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{CreateSessionOptions, Session};
use bohio_proto::gui::{Button, ButtonAction, Display, DisplayId, InputEvent, KeyAction, Rect};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::Pod;
use xkeysym::{Keysym, key};

use super::logind::Logind;
use super::x11::{display_id, keysym_of, one_primary};
use crate::gui::{Capabilities, Desktop, DesktopState, Frame, GuiError};

/// What a node says when the portal has no RemoteDesktop interface.
const NO_INPUT: &str = "the desktop portal has no RemoteDesktop interface, so this session can be viewed and not controlled";

/// What a capture says when the compositor has sent no picture of a monitor in time.
const NO_PICTURE: &str = "the compositor has sent no picture of this monitor yet: xdg-desktop-portal-wlr sends one only when the \
     screen changes, so a screen that does not change gives none (wayland_capture_wait is how long a request waits)";

/// What a capture says when the session ended under it.
const ENDED: &str = "the portal's screen sharing session ended (the compositor or its portal stopped, or the person stopped the share): the next request asks the portal again";

/// What the first request meets while the portal's dialog is open.
const PENDING: &str = "the desktop portal is waiting for the person at the desktop to answer its screen sharing dialog; ask again once it is answered";

/// Whether and how long the portal's approval is remembered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Persist {
    /// The portal asks every time a session starts.
    None,
    /// The portal asks once for each run of the node (the session is kept while the node runs). The default.
    #[default]
    Node,
    /// The portal gives a restore token that the node keeps in its data directory (`0600`) and offers at the next start,
    /// so a portal that honours it asks once for good.
    Always,
}

impl Persist {
    /// The setting's word: `none`, `node` or `always`.
    pub fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "none" => Self::None,
            "node" => Self::Node,
            "always" => Self::Always,
            _ => return None,
        })
    }

    fn mode(self) -> PersistMode {
        match self {
            Self::None => PersistMode::DoNot,
            Self::Node => PersistMode::Application,
            Self::Always => PersistMode::ExplicitlyRevoked,
        }
    }
}

/// How the Wayland backend runs; the `native` source builds it from `[source.native]`.
#[derive(Debug, Clone)]
pub struct WaylandSettings {
    /// Whether the portal remembers its approval.
    pub persist: Persist,
    /// Where the restore token is kept when `persist` is [`Persist::Always`]; without a place it is not kept.
    pub restore_file: Option<PathBuf>,
    /// How long a request waits for a monitor's first picture once the portal has started the session.
    pub capture_wait: Duration,
    /// How long a request waits for the portal's dialog to be answered before it reports the dialog as pending (the
    /// session goes on starting in the background).
    pub start_wait: Duration,
    /// How long after a refused or failed start the next request does not ask the portal again, so that a peer asking
    /// `info` cannot make the dialog appear over and over.
    pub retry: Duration,
    /// The login session logind is asked about (`XDG_SESSION_ID`); `None` asks for this process's own.
    pub session: Option<String>,
    /// The address of the system bus, when it is not the standard one.
    pub system_bus: Option<String>,
    /// How long logind may take to answer.
    pub bus_wait: Duration,
    /// A desktop of the node's own that no person sits at: it has no lock screen, so it is never locked and does not ask
    /// logind.
    pub headless: bool,
    /// The most clicks of one button event and wheel notches of one scroll event; more is `bad_request`.
    pub max_repeat: u32,
}

impl Default for WaylandSettings {
    fn default() -> Self {
        Self {
            persist: Persist::Node,
            restore_file: None,
            capture_wait: Duration::from_secs(5),
            start_wait: Duration::from_secs(10),
            retry: Duration::from_secs(30),
            session: None,
            system_bus: None,
            bus_wait: Duration::from_secs(1),
            headless: false,
            max_repeat: 1000,
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

// ----------------------------------------------------------------------------------------------------------------
// Pixels.

/// Where each colour sits in a pixel's bytes, in memory order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Layout {
    /// Bytes in a pixel.
    pub bytes: usize,
    pub red: usize,
    pub green: usize,
    pub blue: usize,
}

/// The formats the stream is offered, the first being the compositor's pick when it has none: the 32-bit ones first
/// (what a compositor draws in), then the 24-bit ones.
fn named() -> [(VideoFormat, &'static str); 10] {
    [
        (VideoFormat::BGRx, "BGRx"),
        (VideoFormat::BGRA, "BGRA"),
        (VideoFormat::RGBx, "RGBx"),
        (VideoFormat::RGBA, "RGBA"),
        (VideoFormat::xRGB, "xRGB"),
        (VideoFormat::ARGB, "ARGB"),
        (VideoFormat::xBGR, "xBGR"),
        (VideoFormat::ABGR, "ABGR"),
        (VideoFormat::BGR, "BGR"),
        (VideoFormat::RGB, "RGB"),
    ]
}

fn offered() -> [VideoFormat; 10] {
    named().map(|(format, _)| format)
}

/// The layout of a format named by its SPA name (`BGRx`: the bytes in memory order, `x` and `A` unused).
pub(crate) fn layout_of_name(name: &str) -> Option<Layout> {
    let letters: Vec<char> = name.chars().collect();
    if !(letters.len() == 3 || letters.len() == 4) {
        return None;
    }
    let find = |colour: char| letters.iter().position(|&letter| letter == colour);
    let (red, green, blue) = (find('R')?, find('G')?, find('B')?);
    let others = letters
        .iter()
        .enumerate()
        .filter(|(index, _)| ![red, green, blue].contains(index))
        .all(|(_, &letter)| letter == 'x' || letter == 'A');
    others.then_some(Layout {
        bytes: letters.len(),
        red,
        green,
        blue,
    })
}

fn layout_of(format: VideoFormat) -> Option<Layout> {
    named()
        .into_iter()
        .find(|(candidate, _)| *candidate == format)
        .and_then(|(_, name)| layout_of_name(name))
}

/// The RGB bytes of `region` of a picture of `width` by `height` pixels whose rows are `stride` bytes apart.
///
/// # Errors
///
/// `io` for data shorter than the rows say, or a region outside the picture.
pub(crate) fn rgb_of(
    data: &[u8],
    stride: usize,
    width: u32,
    height: u32,
    layout: Layout,
    region: Rect,
) -> Result<Vec<u8>, GuiError> {
    let inside = u64::from(region.x) + u64::from(region.width) <= u64::from(width)
        && u64::from(region.y) + u64::from(region.height) <= u64::from(height);
    if !inside {
        return Err(GuiError::bad_request("the region lies outside the picture"));
    }
    let row_bytes = width as usize * layout.bytes;
    let needed = (height as usize - 1) * stride + row_bytes;
    if stride < row_bytes || data.len() < needed {
        return Err(GuiError::io(
            "the compositor sent a picture shorter than its size",
        ));
    }
    let mut rgb = Vec::with_capacity(region.width as usize * region.height as usize * 3);
    for row in region.y..region.y + region.height {
        let start = row as usize * stride + region.x as usize * layout.bytes;
        for pixel in data[start..start + region.width as usize * layout.bytes].chunks(layout.bytes)
        {
            rgb.extend_from_slice(&[pixel[layout.red], pixel[layout.green], pixel[layout.blue]]);
        }
    }
    Ok(rgb)
}

// ----------------------------------------------------------------------------------------------------------------
// Layout.

/// What the portal says of one monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Source {
    /// The PipeWire node the monitor's picture comes from, and the id input names it by.
    pub node: u32,
    /// The portal's name for the source, when it gives one.
    pub name: Option<String>,
    /// Where the monitor is in the compositor's logical layout.
    pub position: Option<(i32, i32)>,
    /// The monitor's size in the compositor's logical units.
    pub logical: Option<(u32, u32)>,
}

/// A monitor as the desktop shows it: the source and the picture's size (known once its first frame arrived).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Shown {
    pub source: Source,
    pub display: Display,
}

/// The displays of `sources` whose pictures are `sizes` (pixels, in the same order; `None` for one that sent no picture).
/// A monitor with neither a picture nor a logical size is left out; one the portal did not place is put right of the
/// last. The first is primary: a portal does not say which monitor is.
pub(crate) fn layout(sources: &[Source], sizes: &[Option<(u32, u32)>]) -> Vec<Shown> {
    let mut shown: Vec<Shown> = Vec::new();
    let mut taken: Vec<String> = Vec::new();
    let mut next_x = 0_i32;
    for (index, source) in sources.iter().enumerate() {
        let size = sizes.get(index).copied().flatten().or(source.logical);
        let Some((width, height)) = size else {
            continue;
        };
        let scale = match source.logical {
            Some((logical, _)) if logical > 0 && width > 0 => f64::from(width) / f64::from(logical),
            _ => 1.0,
        };
        let (x, y) = source.position.unwrap_or((next_x, 0));
        let logical_width = source.logical.map_or(width, |(logical, _)| logical);
        next_x = x.saturating_add(i32::try_from(logical_width).unwrap_or(i32::MAX));
        let name = source.name.clone().unwrap_or_else(|| "monitor".to_owned());
        let id = display_id(&name, index, &taken);
        taken.push(id.clone());
        shown.push(Shown {
            source: source.clone(),
            display: Display {
                id: DisplayId(id),
                name: source.name.clone(),
                primary: false,
                x,
                y,
                width,
                height,
                scale,
                rotation: None,
            },
        });
    }
    let mut displays: Vec<Display> = shown.iter().map(|each| each.display.clone()).collect();
    one_primary(&mut displays);
    for (each, display) in shown.iter_mut().zip(displays) {
        each.display = display;
    }
    shown
}

// ----------------------------------------------------------------------------------------------------------------
// Input.

/// The Linux input event code of a mouse button (`BTN_LEFT` and the others; the portal takes evdev codes).
const fn button_code(button: Button) -> i32 {
    match button {
        Button::Left => 0x110,
        Button::Right => 0x111,
        Button::Middle => 0x112,
        // BTN_SIDE and BTN_EXTRA: what every desktop treats as back and forward.
        Button::Back => 0x113,
        Button::Forward => 0x114,
    }
}

/// One thing sent to the portal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Step {
    /// The pointer to a point of a monitor, in the compositor's logical units from its top-left.
    Motion {
        node: u32,
        x: f64,
        y: f64,
    },
    Button {
        code: i32,
        down: bool,
    },
    /// Wheel notches on an axis; a positive count scrolls down or to the right.
    Axis {
        vertical: bool,
        steps: i32,
    },
    Key {
        sym: i32,
        down: bool,
    },
}

fn motion(shown: &[Shown], display: Option<&DisplayId>, x: i32, y: i32) -> Result<Step, GuiError> {
    let found = match display {
        Some(id) => shown.iter().find(|each| &each.display.id == id),
        None => shown
            .iter()
            .find(|each| each.display.primary)
            .or_else(|| shown.first()),
    }
    .ok_or_else(|| GuiError::not_found("no such display"))?;
    let (width, height) = (found.display.width, found.display.height);
    let inside =
        x >= 0 && y >= 0 && i64::from(x) < i64::from(width) && i64::from(y) < i64::from(height);
    if !inside {
        return Err(GuiError::bad_request(format!(
            "the point lies outside the display ({width} by {height} pixels)"
        )));
    }
    let scale = if found.display.scale > 0.0 {
        found.display.scale
    } else {
        1.0
    };
    Ok(Step::Motion {
        node: found.source.node,
        x: f64::from(x) / scale,
        y: f64::from(y) / scale,
    })
}

fn keysym_i32(sym: u32) -> Result<i32, GuiError> {
    i32::try_from(sym).map_err(|_| GuiError::bad_request("a key the portal cannot name"))
}

fn key_steps(sym: u32, action: KeyAction) -> Result<Vec<Step>, GuiError> {
    let sym = keysym_i32(sym)?;
    let mut steps = Vec::new();
    if action != KeyAction::Release {
        steps.push(Step::Key { sym, down: true });
    }
    if action != KeyAction::Press {
        steps.push(Step::Key { sym, down: false });
    }
    Ok(steps)
}

fn text_steps(text: &str) -> Result<Vec<Step>, GuiError> {
    let mut steps = Vec::new();
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        let sym = match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                key::Return
            }
            '\n' => key::Return,
            '\t' => key::Tab,
            other => {
                let sym = Keysym::from_char(other);
                if sym == Keysym::NoSymbol {
                    return Err(GuiError::bad_request("a character that has no keysym"));
                }
                sym.raw()
            }
        };
        steps.extend(key_steps(sym, KeyAction::Click)?);
    }
    Ok(steps)
}

/// The steps of one event, in order. `wait` has none (the engine pauses itself).
///
/// # Errors
///
/// `bad_request` for a point off its display, a key that is not one, or a repeat past `max_repeat`.
pub(crate) fn plan(
    event: &InputEvent,
    shown: &[Shown],
    max_repeat: u32,
) -> Result<Vec<Step>, GuiError> {
    let repeat = |count: u32| -> Result<u32, GuiError> {
        if count > max_repeat {
            return Err(GuiError::bad_request(format!(
                "that repeats {count} times; this host repeats at most {max_repeat}"
            )));
        }
        Ok(count)
    };
    Ok(match event {
        InputEvent::Move { display, x, y } => vec![motion(shown, display.as_ref(), *x, *y)?],
        InputEvent::Button {
            button,
            action,
            count,
            display,
            x,
            y,
        } => {
            let mut steps = Vec::new();
            if let (Some(x), Some(y)) = (x, y) {
                steps.push(motion(shown, display.as_ref(), *x, *y)?);
            }
            let code = button_code(*button);
            match action {
                ButtonAction::Press => steps.push(Step::Button { code, down: true }),
                ButtonAction::Release => steps.push(Step::Button { code, down: false }),
                ButtonAction::Click => {
                    for _ in 0..repeat(count.unwrap_or(1).max(1))? {
                        steps.push(Step::Button { code, down: true });
                        steps.push(Step::Button { code, down: false });
                    }
                }
            }
            steps
        }
        InputEvent::Scroll {
            display,
            x,
            y,
            dx,
            dy,
        } => {
            let mut steps = Vec::new();
            if let (Some(x), Some(y)) = (x, y) {
                steps.push(motion(shown, display.as_ref(), *x, *y)?);
            }
            for (notches, vertical) in [(*dy, true), (*dx, false)] {
                if notches != 0 {
                    repeat(notches.unsigned_abs())?;
                    steps.push(Step::Axis {
                        vertical,
                        steps: notches,
                    });
                }
            }
            steps
        }
        InputEvent::Key { key, action } => {
            let sym =
                keysym_of(key).ok_or_else(|| GuiError::bad_request("that is not a key name"))?;
            key_steps(sym, *action)?
        }
        InputEvent::Text { text } => text_steps(text)?,
        InputEvent::Wait { .. } => Vec::new(),
    })
}

/// Where steps go: the portal, or a recording in a test.
pub(crate) trait Sink {
    fn send(&self, step: Step) -> Result<(), GuiError>;
}

/// Sends the steps of each event in order and returns how many events were sent whole; a failure after the first event
/// is the count so far, and one in the first is the error.
pub(crate) fn apply(sink: &dyn Sink, planned: &[Vec<Step>]) -> Result<u32, GuiError> {
    let mut applied = 0_u32;
    for steps in planned {
        for step in steps {
            if let Err(error) = sink.send(*step) {
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
}

// ----------------------------------------------------------------------------------------------------------------
// The portal.

fn portal_error(what: &str, error: &ashpd::Error) -> GuiError {
    match error {
        ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled) => {
            GuiError::unavailable(format!(
                "{what}: the person at the desktop refused to share the screen in the portal's dialog"
            ))
        }
        other => GuiError::unavailable(format!("{what}: {other}")),
    }
}

/// An open portal session and what it can do.
struct Portal {
    remote: Option<(RemoteDesktop, Session<RemoteDesktop>)>,
    cast: Option<Session<Screencast>>,
}

impl Portal {
    fn close(&self) {
        pollster::block_on(async {
            if let Some((_, session)) = &self.remote {
                let _ = session.close().await;
            }
            if let Some(session) = &self.cast {
                let _ = session.close().await;
            }
        });
    }
}

impl Sink for Portal {
    fn send(&self, step: Step) -> Result<(), GuiError> {
        let Some((remote, session)) = &self.remote else {
            return Err(GuiError::unsupported(NO_INPUT));
        };
        let state = |down: bool| {
            if down {
                KeyState::Pressed
            } else {
                KeyState::Released
            }
        };
        pollster::block_on(async {
            match step {
                Step::Motion { node, x, y } => {
                    remote
                        .notify_pointer_motion_absolute(session, node, x, y, Default::default())
                        .await
                }
                Step::Button { code, down } => {
                    remote
                        .notify_pointer_button(session, code, state(down), Default::default())
                        .await
                }
                Step::Axis { vertical, steps } => {
                    let axis = if vertical {
                        Axis::Vertical
                    } else {
                        Axis::Horizontal
                    };
                    remote
                        .notify_pointer_axis_discrete(session, axis, steps, Default::default())
                        .await
                }
                Step::Key { sym, down } => {
                    remote
                        .notify_keyboard_keysym(session, sym, state(down), Default::default())
                        .await
                }
            }
        })
        .map_err(|error| GuiError::io(format!("sending input through the portal: {error}")))
    }
}

/// What the portal gave back when the session started.
struct Started {
    portal: Portal,
    sources: Vec<Source>,
    fd: OwnedFd,
    token: Option<String>,
}

fn sources_of(streams: &[ashpd::desktop::screencast::Stream]) -> Vec<Source> {
    streams
        .iter()
        .map(|stream| Source {
            node: stream.pipe_wire_node_id(),
            name: stream.id().map(ToOwned::to_owned),
            position: stream.position(),
            logical: stream.size().and_then(|(width, height)| {
                Some((u32::try_from(width).ok()?, u32::try_from(height).ok()?))
            }),
        })
        .collect()
}

fn read_token(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let token = text.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

/// Keeps the portal's restore token readable by this account only.
fn write_token(path: &Path, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let temporary = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary)?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path)
}

/// Asks the portal for the session: with input when it has a RemoteDesktop, else a view.
async fn open(settings: &WaylandSettings) -> Result<Started, GuiError> {
    let screencast = Screencast::new()
        .await
        .map_err(|error| portal_error("the desktop portal has no ScreenCast", &error))?;
    let persist = settings.persist.mode();
    let token = settings
        .restore_file
        .as_deref()
        .filter(|_| settings.persist == Persist::Always)
        .and_then(read_token);

    let remote = RemoteDesktop::new().await;
    if let Err(error) = &remote {
        tracing::debug!(%error, "the portal has no RemoteDesktop");
    }
    if let Ok(remote) = remote {
        // Everything up to `start` shows no dialog, so a failure there only means this portal cannot take input.
        let prepared = async {
            let session = remote
                .create_session(CreateSessionOptions::default())
                .await?;
            remote
                .select_devices(
                    &session,
                    SelectDevicesOptions::default()
                        .set_devices(DeviceType::KEYBOARD | DeviceType::POINTER)
                        .set_persist_mode(persist)
                        .set_restore_token(token.as_deref()),
                )
                .await?
                .response()?;
            screencast
                .select_sources(
                    &session,
                    SelectSourcesOptions::default()
                        .set_cursor_mode(CursorMode::HIDDEN)
                        .set_sources(SourceType::MONITOR)
                        .set_multiple(true),
                )
                .await?
                .response()?;
            Ok::<_, ashpd::Error>(session)
        }
        .await;
        match prepared {
            Ok(session) => {
                let selected = remote
                    .start(&session, None, Default::default())
                    .await
                    .and_then(|request| request.response())
                    .map_err(|error| portal_error("starting the RemoteDesktop session", &error))?;
                let fd = screencast
                    .open_pipe_wire_remote(&session, Default::default())
                    .await
                    .map_err(|error| portal_error("opening the PipeWire remote", &error))?;
                return Ok(Started {
                    sources: sources_of(selected.streams()),
                    token: selected.restore_token().map(ToOwned::to_owned),
                    portal: Portal {
                        remote: Some((remote, session)),
                        cast: None,
                    },
                    fd,
                });
            }
            Err(error) => tracing::debug!(%error, "no RemoteDesktop session, so a view only"),
        }
    }

    let session = screencast
        .create_session(CreateSessionOptions::default())
        .await
        .map_err(|error| portal_error("creating the ScreenCast session", &error))?;
    screencast
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::HIDDEN)
                .set_sources(SourceType::MONITOR)
                .set_multiple(true)
                .set_persist_mode(persist)
                .set_restore_token(token.as_deref()),
        )
        .await
        .and_then(|request| request.response())
        .map_err(|error| portal_error("selecting the monitors", &error))?;
    let streams = screencast
        .start(&session, None, Default::default())
        .await
        .and_then(|request| request.response())
        .map_err(|error| portal_error("starting the ScreenCast session", &error))?;
    let fd = screencast
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(|error| portal_error("opening the PipeWire remote", &error))?;
    Ok(Started {
        sources: sources_of(streams.streams()),
        token: streams.restore_token().map(ToOwned::to_owned),
        portal: Portal {
            remote: None,
            cast: Some(session),
        },
        fd,
    })
}

// ----------------------------------------------------------------------------------------------------------------
// PipeWire.

/// A frame as the compositor drew it.
struct Raw {
    width: u32,
    height: u32,
    stride: usize,
    layout: Layout,
    data: Vec<u8>,
}

#[derive(Default)]
struct Slot {
    /// The size in pixels the compositor negotiated, known before its first picture.
    size: Option<(u32, u32)>,
    frame: Option<Raw>,
    /// Why this stream gives no pictures, when it does not.
    failure: Option<String>,
}

struct Shared {
    slots: Mutex<Vec<Slot>>,
    changed: Condvar,
    /// The streams are gone: the portal's session ended under this node, or the remote closed.
    closed: AtomicBool,
}

impl Shared {
    fn fail(&self, index: usize, why: &str) {
        let mut slots = lock(&self.slots);
        if let Some(slot) = slots.get_mut(index)
            && slot.failure.is_none()
        {
            slot.failure = Some(why.to_owned());
        }
        self.changed.notify_all();
    }
}

struct StreamData {
    index: usize,
    shared: Arc<Shared>,
    info: VideoInfoRaw,
}

/// The format a stream is asked for: any of the offered layouts, any size. No modifier is named, so the buffers are
/// memory the process can map, never a GPU handle.
fn format_param() -> Result<Vec<u8>, String> {
    let formats = offered();
    let object = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            formats[0],
            formats[0],
            formats[1],
            formats[2],
            formats[3],
            formats[4],
            formats[5],
            formats[6],
            formats[7],
            formats[8],
            formats[9],
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle {
                width: 1280,
                height: 720
            },
            spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            spa::utils::Rectangle {
                width: 16384,
                height: 16384
            }
        ),
        spa::pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 30, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );
    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .map(|(cursor, _)| cursor.into_inner())
    .map_err(|error| format!("building the stream's format: {error:?}"))
}

/// Runs the PipeWire loop that reads every monitor's stream, until `quit` is sent or the portal closes the remote.
fn run_streams(
    fd: OwnedFd,
    nodes: &[u32],
    shared: &Arc<Shared>,
    quit: pw::channel::Receiver<()>,
    ready: &mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let describe = |what: &str, error: pw::Error| format!("{what}: {error}");
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|error| describe("creating the PipeWire loop", error))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|error| describe("creating the PipeWire context", error))?;
    let core = context
        .connect_fd_rc(fd, None)
        .map_err(|error| describe("connecting to the portal's PipeWire remote", error))?;
    let _core_listener = core
        .add_listener_local()
        .error({
            let (shared, mainloop) = (Arc::clone(shared), mainloop.clone());
            move |_, _, _, message| {
                tracing::debug!(message, "the PipeWire remote reported an error");
                shared.closed.store(true, Ordering::SeqCst);
                shared.changed.notify_all();
                mainloop.quit();
            }
        })
        .register();
    let param = format_param()?;
    let mut keep = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        let stream = pw::stream::StreamBox::new(
            &core,
            "bohio",
            properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
            },
        )
        .map_err(|error| describe("creating a stream", error))?;
        let listener = stream
            .add_local_listener_with_user_data(StreamData {
                index,
                shared: Arc::clone(shared),
                info: VideoInfoRaw::default(),
            })
            .state_changed(|_, data, _, state| {
                if let pw::stream::StreamState::Error(message) = state {
                    data.shared.fail(data.index, &message);
                }
            })
            .param_changed(|_, data, id, param| {
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let Some(param) = param else {
                    // PipeWire clears the format when the node goes away (the compositor or its portal ended, or the
                    // person stopped the share): the session is over, and the next request makes another.
                    data.shared.closed.store(true, Ordering::SeqCst);
                    data.shared.changed.notify_all();
                    return;
                };
                let Ok((media, subtype)) = spa::param::format_utils::parse_format(param) else {
                    return;
                };
                if media != spa::param::format::MediaType::Video
                    || subtype != spa::param::format::MediaSubtype::Raw
                {
                    return;
                }
                if data.info.parse(param).is_err() {
                    return;
                }
                if layout_of(data.info.format()).is_none() {
                    data.shared.fail(
                        data.index,
                        "the compositor chose a pixel format this host cannot read",
                    );
                    return;
                }
                let size = data.info.size();
                let mut slots = lock(&data.shared.slots);
                if let Some(slot) = slots.get_mut(data.index) {
                    slot.size = Some((size.width, size.height));
                }
                data.shared.changed.notify_all();
            })
            .process(|stream, data| {
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let Some(layout) = layout_of(data.info.format()) else {
                    return;
                };
                let size = data.info.size();
                let Some(first) = buffer.datas_mut().first_mut() else {
                    return;
                };
                let (offset, used, stride) = {
                    let chunk = first.chunk();
                    (
                        chunk.offset() as usize,
                        chunk.size() as usize,
                        chunk.stride(),
                    )
                };
                // An empty buffer or one marked corrupt carries no new picture: keep the last one.
                if used == 0
                    || first
                        .chunk()
                        .flags()
                        .contains(spa::buffer::ChunkFlags::CORRUPTED)
                {
                    return;
                }
                let Some(bytes) = first.data() else {
                    data.shared.fail(
                        data.index,
                        "the compositor sent a GPU buffer, which this host cannot read",
                    );
                    return;
                };
                let end = offset.saturating_add(used).min(bytes.len());
                let stride = usize::try_from(stride)
                    .ok()
                    .filter(|stride| *stride > 0)
                    .unwrap_or(size.width as usize * layout.bytes);
                let mut slots = lock(&data.shared.slots);
                let Some(slot) = slots.get_mut(data.index) else {
                    return;
                };
                let mut copy = slot.frame.take().map(|old| old.data).unwrap_or_default();
                copy.clear();
                copy.extend_from_slice(&bytes[offset.min(end)..end]);
                slot.frame = Some(Raw {
                    width: size.width,
                    height: size.height,
                    stride,
                    layout,
                    data: copy,
                });
                data.shared.changed.notify_all();
            })
            .register()
            .map_err(|error| describe("listening to a stream", error))?;
        let mut params = [Pod::from_bytes(&param).ok_or("the stream's format is not a pod")?];
        stream
            .connect(
                spa::utils::Direction::Input,
                Some(*node),
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut params,
            )
            .map_err(|error| describe("connecting a stream", error))?;
        keep.push((stream, listener));
    }
    let _quit = quit.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |()| mainloop.quit()
    });
    let _ = ready.send(Ok(()));
    mainloop.run();
    drop(keep);
    Ok(())
}

// ----------------------------------------------------------------------------------------------------------------
// The session.

/// A started session: the portal, the monitors and the thread that reads their pictures.
struct Live {
    portal: Portal,
    shared: Arc<Shared>,
    sources: Vec<Source>,
    quit: Mutex<Option<pw::channel::Sender<()>>>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Live {
    fn build(started: Started, settings: &WaylandSettings) -> Result<Self, GuiError> {
        if let (Some(token), Some(path)) = (&started.token, &settings.restore_file)
            && settings.persist == Persist::Always
            && let Err(error) = write_token(path, token)
        {
            tracing::warn!(%error, "the portal's restore token was not kept");
        }
        let Started {
            portal,
            sources,
            fd,
            ..
        } = started;
        if sources.is_empty() {
            portal.close();
            return Err(GuiError::unavailable(
                "the portal started the session with no monitor to capture",
            ));
        }
        let shared = Arc::new(Shared {
            slots: Mutex::new(sources.iter().map(|_| Slot::default()).collect()),
            changed: Condvar::new(),
            closed: AtomicBool::new(false),
        });
        let (quit_send, quit_receive) = pw::channel::channel::<()>();
        let (ready_send, ready_receive) = mpsc::channel();
        let nodes: Vec<u32> = sources.iter().map(|source| source.node).collect();
        let worker = thread::Builder::new()
            .name("bohio-wayland".to_owned())
            .spawn({
                let shared = Arc::clone(&shared);
                let ready = ready_send;
                move || {
                    if let Err(error) = run_streams(fd, &nodes, &shared, quit_receive, &ready) {
                        let _ = ready.send(Err(error));
                    }
                    shared.closed.store(true, Ordering::SeqCst);
                    shared.changed.notify_all();
                }
            })
            .map_err(|error| GuiError::io(format!("starting the PipeWire thread: {error}")))?;
        let live = Self {
            portal,
            shared,
            sources,
            quit: Mutex::new(Some(quit_send)),
            worker: Mutex::new(Some(worker)),
        };
        match ready_receive.recv_timeout(settings.capture_wait) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(GuiError::unavailable(error)),
            Err(_) => {
                return Err(GuiError::unavailable(
                    "PipeWire did not connect to the portal's remote in time",
                ));
            }
        }
        // Every monitor's size is known once the compositor has negotiated its stream: before its first picture, which
        // a compositor that sends one only when the screen changes may not send for a while.
        let until = Instant::now() + settings.capture_wait;
        let mut slots = lock(&live.shared.slots);
        while slots
            .iter()
            .any(|slot| slot.size.is_none() && slot.failure.is_none())
            && !live.shared.closed.load(Ordering::SeqCst)
        {
            let Some(left) = until.checked_duration_since(Instant::now()) else {
                break;
            };
            slots = live
                .shared
                .changed
                .wait_timeout(slots, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        let any_size = slots.iter().any(|slot| slot.size.is_some());
        let failure = slots.iter().find_map(|slot| slot.failure.clone());
        drop(slots);
        if !any_size {
            return Err(GuiError::unavailable(failure.unwrap_or_else(|| {
                "no monitor's stream was negotiated within wayland_capture_wait".to_owned()
            })));
        }
        Ok(live)
    }

    fn is_closed(&self) -> bool {
        self.shared.closed.load(Ordering::SeqCst)
    }

    fn shown(&self) -> Vec<Shown> {
        let sizes: Vec<Option<(u32, u32)>> = lock(&self.shared.slots)
            .iter()
            .map(|slot| slot.size)
            .collect();
        layout(&self.sources, &sizes)
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Some(quit) = lock(&self.quit).take() {
            let _ = quit.send(());
        }
        if let Some(worker) = lock(&self.worker).take() {
            let _ = worker.join();
        }
        self.portal.close();
    }
}

enum Phase {
    Idle,
    Starting,
    Live(Arc<Live>),
    Failed { at: Instant, error: GuiError },
}

struct Inner {
    settings: WaylandSettings,
    phase: Mutex<Phase>,
    changed: Condvar,
    logind: Logind,
    pointer: Mutex<Option<(DisplayId, i32, i32)>>,
    /// Whether the portal offers input, asked once while no session says.
    input_offered: Mutex<Option<bool>>,
}

impl Inner {
    /// The live session, started if there is none. A first request waits `start_wait` for the portal's dialog and
    /// reports it pending after that; a failed start is reported until `retry` has passed.
    fn ensure(self: &Arc<Self>) -> Result<Arc<Live>, GuiError> {
        let until = Instant::now() + self.settings.start_wait;
        let mut phase = lock(&self.phase);
        loop {
            match &*phase {
                Phase::Live(live) if live.is_closed() => *phase = Phase::Idle,
                Phase::Live(live) => return Ok(Arc::clone(live)),
                Phase::Failed { at, error } => {
                    if at.elapsed() < self.settings.retry {
                        return Err(error.clone());
                    }
                    *phase = Phase::Idle;
                }
                Phase::Idle => {
                    *phase = Phase::Starting;
                    let inner = Arc::clone(self);
                    let spawned = thread::Builder::new()
                        .name("bohio-wayland-start".to_owned())
                        .spawn(move || {
                            let result = pollster::block_on(open(&inner.settings))
                                .and_then(|started| Live::build(started, &inner.settings));
                            *lock(&inner.phase) = match result {
                                Ok(live) => Phase::Live(Arc::new(live)),
                                Err(error) => Phase::Failed {
                                    at: Instant::now(),
                                    error,
                                },
                            };
                            inner.changed.notify_all();
                        });
                    if let Err(error) = spawned {
                        let error = GuiError::io(format!("starting the portal request: {error}"));
                        *phase = Phase::Failed {
                            at: Instant::now(),
                            error: error.clone(),
                        };
                        return Err(error);
                    }
                }
                Phase::Starting => {
                    let Some(left) = until.checked_duration_since(Instant::now()) else {
                        return Err(GuiError::unavailable(PENDING));
                    };
                    phase = self
                        .changed
                        .wait_timeout(phase, left)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            }
        }
    }

    fn state_of(&self, live: &Live) -> DesktopState {
        DesktopState {
            user: None,
            active: true,
            locked: if self.settings.headless {
                Some(false)
            } else {
                self.logind.hints().map(|hints| hints.locked)
            },
            displays: live.shown().into_iter().map(|each| each.display).collect(),
        }
    }
}

/// The screen of a Wayland session, served as a [`Desktop`] through the desktop portals.
///
/// The portal session is made at the first call and again after the compositor or the person ends it. Without a
/// RemoteDesktop portal the desktop is a view: input is `unsupported`.
pub struct WaylandDesktop {
    inner: Arc<Inner>,
}

impl WaylandDesktop {
    /// A desktop run by `settings`. Nothing is asked of the portal until the desktop is asked something.
    #[must_use]
    pub fn new(settings: WaylandSettings) -> Self {
        let logind = Logind::new(
            settings.system_bus.clone(),
            settings.session.clone(),
            settings.bus_wait,
        );
        Self {
            inner: Arc::new(Inner {
                settings,
                phase: Mutex::new(Phase::Idle),
                changed: Condvar::new(),
                logind,
                pointer: Mutex::new(None),
                input_offered: Mutex::new(None),
            }),
        }
    }
}

impl Desktop for WaylandDesktop {
    fn state(&self) -> Result<DesktopState, GuiError> {
        let live = self.inner.ensure()?;
        Ok(self.inner.state_of(&live))
    }

    fn capture(&self, display: &DisplayId, region: Option<Rect>) -> Result<Frame, GuiError> {
        let live = self.inner.ensure()?;
        let shown = live.shown();
        let found = shown
            .iter()
            .find(|each| &each.display.id == display)
            .ok_or_else(|| GuiError::not_found("no such display"))?;
        let index = live
            .sources
            .iter()
            .position(|source| source.node == found.source.node)
            .ok_or_else(|| GuiError::not_found("no such display"))?;
        // A compositor may send a picture only when the screen changes (`xdg-desktop-portal-wlr` does): wait for the
        // first one, for as long as `capture_wait` says.
        let until = Instant::now() + self.inner.settings.capture_wait;
        let mut slots = lock(&live.shared.slots);
        loop {
            let slot = slots
                .get(index)
                .ok_or_else(|| GuiError::not_found("no such display"))?;
            if let Some(failure) = &slot.failure {
                return Err(GuiError::unavailable(failure.clone()));
            }
            if live.is_closed() {
                return Err(GuiError::unavailable(ENDED));
            }
            if slot.frame.is_some() {
                break;
            }
            let left = until
                .checked_duration_since(Instant::now())
                .ok_or_else(|| GuiError::unavailable(NO_PICTURE))?;
            slots = live
                .shared
                .changed
                .wait_timeout(slots, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        let frame = slots
            .get(index)
            .and_then(|slot| slot.frame.as_ref())
            .ok_or_else(|| GuiError::unavailable(NO_PICTURE))?;
        let region = region.unwrap_or_else(|| Rect::new(0, 0, frame.width, frame.height));
        let rgb = rgb_of(
            &frame.data,
            frame.stride,
            frame.width,
            frame.height,
            frame.layout,
            region,
        )?;
        drop(slots);
        Frame::new(region.width, region.height, rgb)
    }

    fn input(&self, events: &[InputEvent]) -> Result<u32, GuiError> {
        let live = self.inner.ensure()?;
        if live.portal.remote.is_none() {
            return Err(GuiError::unsupported(NO_INPUT));
        }
        if self.inner.state_of(&live).locked == Some(true) {
            return Err(GuiError::unavailable(
                "the session is locked: input is not sent to a lock screen",
            ));
        }
        let shown = live.shown();
        // Every event is planned before any is sent, so one that names a point off its display refuses the batch whole.
        let planned: Vec<Vec<Step>> = events
            .iter()
            .map(|event| plan(event, &shown, self.inner.settings.max_repeat))
            .collect::<Result<_, _>>()?;
        let applied = apply(&live.portal, &planned)?;
        for event in events.iter().take(applied as usize) {
            let (display, x, y) = match event {
                InputEvent::Move { display, x, y } => (display, Some(*x), Some(*y)),
                InputEvent::Button { display, x, y, .. }
                | InputEvent::Scroll { display, x, y, .. } => (display, *x, *y),
                _ => continue,
            };
            if let (Some(x), Some(y)) = (x, y)
                && let Some(found) = display
                    .as_ref()
                    .and_then(|id| shown.iter().find(|each| &each.display.id == id))
                    .or_else(|| shown.iter().find(|each| each.display.primary))
            {
                *lock(&self.inner.pointer) = Some((found.display.id.clone(), x, y));
            }
        }
        Ok(applied)
    }

    fn cursor(&self) -> Option<(DisplayId, i32, i32)> {
        lock(&self.inner.pointer).clone()
    }

    fn clipboard_text(&self) -> Result<String, GuiError> {
        Err(GuiError::unsupported(
            "the Wayland backend has no clipboard",
        ))
    }

    fn set_clipboard_text(&self, _text: &str) -> Result<(), GuiError> {
        Err(GuiError::unsupported(
            "the Wayland backend has no clipboard",
        ))
    }

    fn capabilities(&self) -> Capabilities {
        let live_input = match &*lock(&self.inner.phase) {
            Phase::Live(live) => Some(live.portal.remote.is_some()),
            _ => None,
        };
        let input = live_input.unwrap_or_else(|| {
            let mut asked = lock(&self.inner.input_offered);
            *asked.get_or_insert_with(|| {
                pollster::block_on(async {
                    match RemoteDesktop::new().await {
                        Ok(remote) => remote
                            .available_device_types()
                            .await
                            .is_ok_and(|types| types.contains(DeviceType::POINTER)),
                        Err(_) => false,
                    }
                })
            })
        });
        Capabilities {
            backend: "wayland".to_owned(),
            input,
            clipboard: false,
            lock_detection: !self.inner.settings.headless && self.inner.logind.hints().is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(node: u32, position: Option<(i32, i32)>, logical: Option<(u32, u32)>) -> Source {
        Source {
            node,
            name: None,
            position,
            logical,
        }
    }

    fn shown_two() -> Vec<Shown> {
        layout(
            &[
                source(41, Some((0, 0)), Some((1280, 720))),
                source(42, Some((1280, 100)), Some((960, 540))),
            ],
            &[Some((1280, 720)), Some((1920, 1080))],
        )
    }

    #[test]
    fn a_bgrx_frame_is_rgb_at_the_formats_byte_offsets() {
        let layout = layout_of_name("BGRx").expect("BGRx");
        assert_eq!(
            layout,
            Layout {
                bytes: 4,
                red: 2,
                green: 1,
                blue: 0
            }
        );
        // Two pixels of two rows, rows eight bytes apart with four bytes of padding after the pixels.
        let data = [
            3, 2, 1, 0, 6, 5, 4, 0, 0, 0, 0, 0, //
            9, 8, 7, 0, 12, 11, 10, 0, 0, 0, 0, 0,
        ];
        let rgb = rgb_of(&data, 12, 2, 2, layout, Rect::new(0, 0, 2, 2)).expect("rgb");
        assert_eq!(rgb, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        let part = rgb_of(&data, 12, 2, 2, layout, Rect::new(1, 1, 1, 1)).expect("a pixel");
        assert_eq!(part, [10, 11, 12]);
    }

    #[test]
    fn every_offered_format_has_a_layout_and_the_colour_letters_name_the_bytes() {
        for format in offered() {
            assert!(layout_of(format).is_some(), "{format:?}");
        }
        let argb = layout_of_name("ARGB").expect("ARGB");
        assert_eq!((argb.red, argb.green, argb.blue, argb.bytes), (1, 2, 3, 4));
        let rgb = layout_of_name("RGB").expect("RGB");
        assert_eq!((rgb.red, rgb.green, rgb.blue, rgb.bytes), (0, 1, 2, 3));
        let bgr = layout_of_name("BGR").expect("BGR");
        assert_eq!((bgr.red, bgr.green, bgr.blue, bgr.bytes), (2, 1, 0, 3));
        assert!(layout_of_name("YUY2").is_none());
        assert!(layout_of_name("RGBR").is_none());
    }

    #[test]
    fn a_short_picture_or_a_region_off_it_is_refused() {
        let layout = layout_of_name("RGBx").expect("RGBx");
        let short = [0_u8; 15];
        let error = rgb_of(&short, 8, 2, 2, layout, Rect::new(0, 0, 2, 2)).unwrap_err();
        assert_eq!(error.code, bohio_proto::ErrorCode::Io);
        let enough = [0_u8; 16];
        let error = rgb_of(&enough, 8, 2, 2, layout, Rect::new(1, 0, 2, 1)).unwrap_err();
        assert_eq!(error.code, bohio_proto::ErrorCode::BadRequest);
        // A stride below the row's own bytes cannot be a picture.
        let error = rgb_of(&enough, 4, 2, 2, layout, Rect::new(0, 0, 2, 2)).unwrap_err();
        assert_eq!(error.code, bohio_proto::ErrorCode::Io);
    }

    #[test]
    fn the_layout_scales_by_the_pictures_size_over_the_logical_size() {
        let shown = shown_two();
        assert_eq!(shown.len(), 2);
        assert!(shown[0].display.primary && !shown[1].display.primary);
        assert!((shown[0].display.scale - 1.0).abs() < f64::EPSILON);
        assert!((shown[1].display.scale - 2.0).abs() < f64::EPSILON);
        assert_eq!((shown[1].display.x, shown[1].display.y), (1280, 100));
        assert_eq!(
            (shown[1].display.width, shown[1].display.height),
            (1920, 1080)
        );
        assert_ne!(shown[0].display.id, shown[1].display.id);
    }

    #[test]
    fn a_monitor_the_portal_did_not_place_goes_right_of_the_last_and_one_with_no_size_is_left_out()
    {
        let shown = layout(
            &[
                source(1, None, Some((800, 600))),
                source(2, None, None),
                source(3, None, Some((640, 480))),
            ],
            &[None, None, None],
        );
        assert_eq!(shown.len(), 2);
        assert_eq!((shown[0].display.x, shown[1].display.x), (0, 800));
    }

    #[test]
    fn a_pixel_is_a_logical_point_of_its_monitor() {
        let shown = shown_two();
        let step = motion(&shown, Some(&shown[1].display.id), 960, 540).expect("a point");
        assert_eq!(
            step,
            Step::Motion {
                node: 42,
                x: 480.0,
                y: 270.0
            }
        );
        let primary = motion(&shown, None, 5, 6).expect("a point");
        assert_eq!(
            primary,
            Step::Motion {
                node: 41,
                x: 5.0,
                y: 6.0
            }
        );
        for (x, y) in [(-1, 0), (0, -1), (1280, 0), (0, 720)] {
            let error = motion(&shown, None, x, y).unwrap_err();
            assert_eq!(error.code, bohio_proto::ErrorCode::BadRequest);
        }
        let error = motion(&shown, Some(&DisplayId("nothing".to_owned())), 0, 0).unwrap_err();
        assert_eq!(error.code, bohio_proto::ErrorCode::NotFound);
    }

    #[test]
    fn a_click_moves_then_presses_and_releases_the_evdev_button() {
        let shown = shown_two();
        let event = InputEvent::Button {
            button: Button::Right,
            action: ButtonAction::Click,
            count: Some(2),
            display: None,
            x: Some(10),
            y: Some(20),
        };
        let steps = plan(&event, &shown, 1000).expect("a plan");
        assert_eq!(
            steps,
            [
                Step::Motion {
                    node: 41,
                    x: 10.0,
                    y: 20.0
                },
                Step::Button {
                    code: 0x111,
                    down: true
                },
                Step::Button {
                    code: 0x111,
                    down: false
                },
                Step::Button {
                    code: 0x111,
                    down: true
                },
                Step::Button {
                    code: 0x111,
                    down: false
                },
            ]
        );
        assert_eq!(button_code(Button::Left), 0x110);
        assert_eq!(button_code(Button::Middle), 0x112);
        assert_eq!(button_code(Button::Back), 0x113);
        assert_eq!(button_code(Button::Forward), 0x114);
    }

    #[test]
    fn a_wheel_is_one_discrete_axis_step_with_the_wires_sign() {
        let shown = shown_two();
        let event = InputEvent::Scroll {
            display: None,
            x: None,
            y: None,
            dx: -2,
            dy: 3,
        };
        assert_eq!(
            plan(&event, &shown, 1000).expect("a plan"),
            [
                Step::Axis {
                    vertical: true,
                    steps: 3
                },
                Step::Axis {
                    vertical: false,
                    steps: -2
                },
            ]
        );
    }

    #[test]
    fn a_repeat_past_the_limit_is_refused_before_anything_is_sent() {
        let shown = shown_two();
        let click = InputEvent::Button {
            button: Button::Left,
            action: ButtonAction::Click,
            count: Some(11),
            display: None,
            x: None,
            y: None,
        };
        assert_eq!(
            plan(&click, &shown, 10).unwrap_err().code,
            bohio_proto::ErrorCode::BadRequest
        );
        let scroll = InputEvent::Scroll {
            display: None,
            x: None,
            y: None,
            dx: 0,
            dy: -11,
        };
        assert_eq!(
            plan(&scroll, &shown, 10).unwrap_err().code,
            bohio_proto::ErrorCode::BadRequest
        );
        assert!(plan(&scroll, &shown, 11).is_ok());
    }

    #[test]
    fn keys_and_text_are_keysyms_pressed_and_released() {
        let shown = shown_two();
        let click = InputEvent::Key {
            key: "enter".to_owned(),
            action: KeyAction::Click,
        };
        let sym = i32::try_from(key::Return).expect("a small keysym");
        assert_eq!(
            plan(&click, &shown, 1000).expect("a plan"),
            [
                Step::Key { sym, down: true },
                Step::Key { sym, down: false }
            ]
        );
        let press = InputEvent::Key {
            key: "shift".to_owned(),
            action: KeyAction::Press,
        };
        assert_eq!(plan(&press, &shown, 1000).expect("a plan").len(), 1);
        let text = InputEvent::Text {
            text: "a\n\u{e9}".to_owned(),
        };
        let steps = plan(&text, &shown, 1000).expect("a plan");
        assert_eq!(steps.len(), 6);
        assert_eq!(
            steps[0],
            Step::Key {
                sym: 0x61,
                down: true
            }
        );
        assert_eq!(steps[2], Step::Key { sym, down: true });
        // A character with a legacy keysym keeps it; the portal maps it to whatever key makes it.
        let wide = plan(
            &InputEvent::Text {
                text: "\u{20ac}".to_owned(),
            },
            &shown,
            1000,
        )
        .expect("a plan");
        assert_eq!(
            wide[0],
            Step::Key {
                sym: 0x20ac,
                down: true
            }
        );
        let bad = InputEvent::Key {
            key: "no_such_key".to_owned(),
            action: KeyAction::Click,
        };
        assert_eq!(
            plan(&bad, &shown, 1000).unwrap_err().code,
            bohio_proto::ErrorCode::BadRequest
        );
        assert!(
            plan(&InputEvent::Wait { ms: 5 }, &shown, 1000)
                .unwrap()
                .is_empty()
        );
    }

    struct Recorder {
        sent: Mutex<Vec<Step>>,
        fail_at: Option<usize>,
    }

    impl Sink for Recorder {
        fn send(&self, step: Step) -> Result<(), GuiError> {
            let mut sent = lock(&self.sent);
            if self.fail_at == Some(sent.len()) {
                return Err(GuiError::io("the portal went away"));
            }
            sent.push(step);
            Ok(())
        }
    }

    #[test]
    fn a_failure_after_the_first_event_counts_the_events_sent_and_in_the_first_is_the_error() {
        let click = |code| {
            vec![
                Step::Button { code, down: true },
                Step::Button { code, down: false },
            ]
        };
        let planned = vec![click(0x110), click(0x111), click(0x112)];
        let whole = Recorder {
            sent: Mutex::new(Vec::new()),
            fail_at: None,
        };
        assert_eq!(apply(&whole, &planned).unwrap(), 3);
        assert_eq!(lock(&whole.sent).len(), 6);
        let later = Recorder {
            sent: Mutex::new(Vec::new()),
            fail_at: Some(3),
        };
        assert_eq!(apply(&later, &planned).unwrap(), 1);
        let first = Recorder {
            sent: Mutex::new(Vec::new()),
            fail_at: Some(0),
        };
        assert_eq!(
            apply(&first, &planned).unwrap_err().code,
            bohio_proto::ErrorCode::Io
        );
    }

    #[test]
    fn persist_words_are_the_settings_and_the_token_file_is_private() {
        assert_eq!(Persist::parse("none"), Some(Persist::None));
        assert_eq!(Persist::parse("node"), Some(Persist::Node));
        assert_eq!(Persist::parse("always"), Some(Persist::Always));
        assert_eq!(Persist::parse("sometimes"), None);
        let dir = tempfile::tempdir().expect("a directory");
        let path = dir.path().join("token");
        write_token(&path, "secret-token").expect("written");
        assert_eq!(read_token(&path).as_deref(), Some("secret-token"));
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        write_token(&path, "replaced").expect("replaced");
        assert_eq!(read_token(&path).as_deref(), Some("replaced"));
        assert!(read_token(&dir.path().join("none")).is_none());
    }
}
