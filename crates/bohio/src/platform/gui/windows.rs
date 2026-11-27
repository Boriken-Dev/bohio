//! The Windows desktop: the screen of the interactive session the node runs in, as a [`Desktop`].
//!
//! Capture is DXGI Desktop Duplication with GDI where duplication cannot serve (`windows_capture`). Input is
//! `SendInput` on the virtual screen, so a display left of or above the main one is reachable. The session's state is the
//! terminal service's connect state and the name of the input desktop, which is how the lock screen and an elevation
//! prompt show. The clipboard's text is `CF_UNICODETEXT`.
//!
//! This module is one of the files that hold `unsafe`.

#![allow(unsafe_code)]

use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    DEVMODEW, DISPLAY_DEVICEW, EnumDisplayDevicesW, EnumDisplayMonitors, EnumDisplaySettingsW,
    GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, CountClipboardFormats, EmptyClipboard, GetClipboardData,
    IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTS_CURRENT_SERVER_HANDLE, WTSActive, WTSConnectState, WTSFreeMemory,
    WTSQuerySessionInformationW, WTSUserName,
};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, GetUserObjectInformationW,
    OpenInputDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForMonitor, MDT_EFFECTIVE_DPI,
    SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, MOUSE_EVENT_FLAGS,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN,
    MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};
use windows::core::{PCWSTR, PWSTR};

use bohio_proto::gui::{Display, DisplayId, InputEvent, Rect};

use super::input::{self, Op, Virtual};
use super::session::{self, Connection, Facts};
use super::windows_capture::{self, Duplication, Target, Timing};
use crate::config::WindowsCapture;
use crate::gui::{Capabilities, Desktop, DesktopState, Frame, GuiError};

// The flag and key values `input` writes out are Windows' own.
const _: () = {
    assert!(input::MOUSEEVENTF_MOVE == MOUSEEVENTF_MOVE.0);
    assert!(input::MOUSEEVENTF_LEFTDOWN == MOUSEEVENTF_LEFTDOWN.0);
    assert!(input::MOUSEEVENTF_LEFTUP == MOUSEEVENTF_LEFTUP.0);
    assert!(input::MOUSEEVENTF_RIGHTDOWN == MOUSEEVENTF_RIGHTDOWN.0);
    assert!(input::MOUSEEVENTF_RIGHTUP == MOUSEEVENTF_RIGHTUP.0);
    assert!(input::MOUSEEVENTF_MIDDLEDOWN == MOUSEEVENTF_MIDDLEDOWN.0);
    assert!(input::MOUSEEVENTF_MIDDLEUP == MOUSEEVENTF_MIDDLEUP.0);
    assert!(input::MOUSEEVENTF_XDOWN == MOUSEEVENTF_XDOWN.0);
    assert!(input::MOUSEEVENTF_XUP == MOUSEEVENTF_XUP.0);
    assert!(input::MOUSEEVENTF_WHEEL == MOUSEEVENTF_WHEEL.0);
    assert!(input::MOUSEEVENTF_HWHEEL == MOUSEEVENTF_HWHEEL.0);
    assert!(input::MOUSEEVENTF_VIRTUALDESK == MOUSEEVENTF_VIRTUALDESK.0);
    assert!(input::MOUSEEVENTF_ABSOLUTE == MOUSEEVENTF_ABSOLUTE.0);
    assert!(input::KEYEVENTF_EXTENDEDKEY == KEYEVENTF_EXTENDEDKEY.0);
    assert!(input::KEYEVENTF_KEYUP == KEYEVENTF_KEYUP.0);
    assert!(input::KEYEVENTF_UNICODE == KEYEVENTF_UNICODE.0);
};

/// How the Windows backend runs; the `native` source builds it from `[source.native]`.
#[derive(Debug, Clone)]
pub struct WindowsSettings {
    /// How the screen is captured.
    pub capture: WindowsCapture,
    /// How long the first capture of a display waits for DXGI's first frame.
    pub frame_wait: Duration,
    /// How long the duplications are kept after the last capture.
    pub capture_idle: Duration,
    /// How long the clipboard is retried while another program holds it.
    pub clipboard_wait: Duration,
    /// The most bytes of clipboard text read or written.
    pub clipboard_max_bytes: usize,
}

impl Default for WindowsSettings {
    fn default() -> Self {
        Self {
            capture: WindowsCapture::Auto,
            frame_wait: Duration::from_millis(250),
            capture_idle: Duration::from_secs(10),
            clipboard_wait: Duration::from_millis(500),
            clipboard_max_bytes: 1024 * 1024,
        }
    }
}

/// A monitor as the operating system lists it.
struct Monitor {
    display: Display,
    /// `\\.\DISPLAY1`.
    device: String,
    rotated: bool,
}

/// The screen of the interactive session this process runs in, served as a [`Desktop`].
///
/// Process-wide, the first one made sets the process to per-monitor DPI awareness (v2), so every coordinate here and on
/// the wire is a physical pixel; a process that was already set keeps its setting. A process in session 0 (a service)
/// has no desktop: [`Desktop::state`] says so and a node serving it reports `available: false`.
pub struct WindowsDesktop {
    settings: WindowsSettings,
    duplication: Duplication,
}

impl WindowsDesktop {
    /// A desktop run by `settings`.
    #[must_use]
    pub fn new(settings: WindowsSettings) -> Self {
        static AWARE: OnceLock<()> = OnceLock::new();
        AWARE.get_or_init(|| {
            // SAFETY: a plain call with a documented constant; it fails (and is ignored) when the process already has an
            // awareness, which is as good.
            let _ = unsafe {
                SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
            };
        });
        let duplication = Duplication::new(Timing {
            frame_wait: settings.frame_wait,
            idle: settings.capture_idle,
        });
        Self {
            settings,
            duplication,
        }
    }

    fn facts() -> Facts {
        let mut session = 0_u32;
        // SAFETY: the out pointer is a local.
        let found = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) };
        if found.is_err() {
            session = 0;
        }
        Facts {
            session,
            connection: connection_of(session),
            input_desktop: input_desktop_name(),
        }
    }

    fn monitors() -> Vec<Monitor> {
        // SAFETY: Windows calls this with the arguments the signature names; it reads only `found`, which is the
        // address `monitors` passes.
        unsafe extern "system" fn each(
            monitor: HMONITOR,
            _: HDC,
            _: *mut RECT,
            found: LPARAM,
        ) -> windows::core::BOOL {
            // SAFETY: `found` is the address of the vector `monitors` passed below, alive for the whole enumeration, which
            // runs on this thread.
            let found = unsafe { &mut *(found.0 as *mut Vec<HMONITOR>) };
            found.push(monitor);
            true.into()
        }
        let mut handles: Vec<HMONITOR> = Vec::new();
        // SAFETY: the callback only pushes onto the vector whose address is passed, which outlives the call.
        let _ = unsafe {
            EnumDisplayMonitors(
                None,
                None,
                Some(each),
                LPARAM(std::ptr::addr_of_mut!(handles) as isize),
            )
        };
        let mut monitors: Vec<Monitor> = handles.into_iter().filter_map(describe).collect();
        monitors.sort_by(|a, b| a.device.cmp(&b.device));
        monitors
    }

    fn state_of(facts: &Facts) -> Result<DesktopState, GuiError> {
        let judged = session::judge(facts)?;
        Ok(DesktopState {
            user: user_of(facts.session),
            active: judged.active,
            locked: Some(judged.locked),
            displays: Self::monitors()
                .into_iter()
                .map(|monitor| monitor.display)
                .collect(),
        })
    }

    /// The virtual screen: the system's own figures, else the displays' union.
    fn virtual_screen(displays: &[Display]) -> Option<Virtual> {
        // SAFETY: plain queries of the system's metrics.
        let (x, y, width, height) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        match (u32::try_from(width), u32::try_from(height)) {
            (Ok(width), Ok(height)) if width > 0 && height > 0 => Some(Virtual {
                x,
                y,
                width,
                height,
            }),
            _ => Virtual::of(displays),
        }
    }
}

impl Desktop for WindowsDesktop {
    fn state(&self) -> Result<DesktopState, GuiError> {
        Self::state_of(&Self::facts())
    }

    fn capture(&self, display: &DisplayId, region: Option<Rect>) -> Result<Frame, GuiError> {
        let facts = Self::facts();
        let judged = session::judge(&facts)?;
        if !judged.active {
            return Err(GuiError::unavailable(
                "the session is not at the screen (it is disconnected or another user's is)",
            ));
        }
        if judged.locked {
            return Err(GuiError::unavailable(session::SECURE_DESKTOP));
        }
        let monitor = Self::monitors()
            .into_iter()
            .find(|monitor| &monitor.display.id == display)
            .ok_or_else(|| GuiError::not_found("no such display"))?;
        let region = region
            .unwrap_or_else(|| Rect::new(0, 0, monitor.display.width, monitor.display.height));
        if !region.fits(monitor.display.width, monitor.display.height) {
            return Err(GuiError::bad_request("the region lies outside the display"));
        }
        let target = Target {
            device: monitor.device,
            origin: (monitor.display.x, monitor.display.y),
            rotated: monitor.rotated,
        };
        match self.settings.capture {
            WindowsCapture::Gdi => windows_capture::gdi(target.origin, region),
            WindowsCapture::Dxgi => self.duplication.grab(&target, region),
            WindowsCapture::Auto => match self.duplication.grab(&target, region) {
                Ok(frame) => Ok(frame),
                Err(error) => {
                    tracing::debug!(%error, "desktop duplication did not serve; capturing with GDI");
                    windows_capture::gdi(target.origin, region)
                }
            },
        }
    }

    fn input(&self, events: &[InputEvent]) -> Result<u32, GuiError> {
        let state = Self::state_of(&Self::facts())?;
        if !state.active {
            return Err(GuiError::unavailable(
                "the session is not at the screen (it is disconnected or another user's is)",
            ));
        }
        // Unlike the other backends this is not left to `[gui] input_when_locked`: the lock screen is the secure desktop, which
        // a process on the user's desktop cannot send input to, so the call would fail whatever the setting.
        if state.locked != Some(false) {
            return Err(GuiError::unavailable(session::SECURE_DESKTOP));
        }
        let virtual_screen = Self::virtual_screen(&state.displays)
            .ok_or_else(|| GuiError::unavailable("there is no display to send input to"))?;
        // Every event is planned before any is sent, so one that names a point off its display refuses the batch whole.
        let planned: Vec<Vec<Op>> = events
            .iter()
            .map(|event| input::plan(event, &state, virtual_screen))
            .collect::<Result<_, _>>()?;
        let mut applied = 0_u32;
        for ops in planned {
            if !ops.is_empty() && send(&ops) != ops.len() {
                if applied == 0 {
                    return Err(GuiError::unavailable(
                        "input was blocked: another desktop has the input, or a higher-integrity window has focus",
                    ));
                }
                return Ok(applied);
            }
            applied += 1;
        }
        Ok(applied)
    }

    fn cursor(&self) -> Option<(DisplayId, i32, i32)> {
        let mut at = POINT::default();
        // SAFETY: the out pointer is a local.
        unsafe { GetCursorPos(&mut at) }.ok()?;
        Self::monitors().into_iter().find_map(|monitor| {
            let display = monitor.display;
            let inside = at.x >= display.x
                && at.y >= display.y
                && i64::from(at.x) < i64::from(display.x) + i64::from(display.width)
                && i64::from(at.y) < i64::from(display.y) + i64::from(display.height);
            inside.then(|| (display.id, at.x - display.x, at.y - display.y))
        })
    }

    fn clipboard_text(&self) -> Result<String, GuiError> {
        let _clipboard = Clipboard::open(self.settings.clipboard_wait)?;
        // SAFETY: the clipboard is open for the calling thread, as these calls require.
        unsafe {
            if CountClipboardFormats() == 0 {
                return Ok(String::new());
            }
            if IsClipboardFormatAvailable(u32::from(CF_UNICODETEXT.0)).is_err() {
                return Err(GuiError::unavailable("the clipboard holds no text"));
            }
            let handle = GetClipboardData(u32::from(CF_UNICODETEXT.0))
                .map_err(|error| io("reading the clipboard", &error))?;
            let memory = HGLOBAL(handle.0);
            let bytes = GlobalSize(memory);
            let units = bytes / 2;
            // Every UTF-16 unit is at least one byte of UTF-8: more units than the limit are more bytes than it.
            if units > self.settings.clipboard_max_bytes.saturating_add(1) {
                return Err(GuiError::bad_request(
                    "the clipboard text is longer than this host reads",
                ));
            }
            let pointer = GlobalLock(memory);
            if pointer.is_null() {
                return Err(GuiError::io("the clipboard's memory could not be locked"));
            }
            // The block is `bytes` long, and the text inside it ends at a NUL or at its end.
            let text = session::text_of_clipboard(std::slice::from_raw_parts(
                pointer.cast::<u16>(),
                units,
            ));
            let _ = GlobalUnlock(memory);
            Ok(text)
        }
    }

    fn set_clipboard_text(&self, text: &str) -> Result<(), GuiError> {
        if text.len() > self.settings.clipboard_max_bytes {
            return Err(GuiError::bad_request(
                "the clipboard text is longer than this host writes",
            ));
        }
        let units = session::clipboard_of_text(text)?;
        let bytes = units.len() * 2;
        let _clipboard = Clipboard::open(self.settings.clipboard_wait)?;
        // SAFETY: the clipboard is open for the calling thread. The block is `bytes` long and written inside its length;
        // it is the clipboard's once `SetClipboardData` succeeds, and freed here when it does not.
        unsafe {
            let memory = GlobalAlloc(GMEM_MOVEABLE, bytes)
                .map_err(|error| io("allocating the clipboard text", &error))?;
            let pointer = GlobalLock(memory);
            if pointer.is_null() {
                let _ = GlobalFree(Some(memory));
                return Err(GuiError::io("the clipboard's memory could not be locked"));
            }
            std::ptr::copy_nonoverlapping(units.as_ptr(), pointer.cast::<u16>(), units.len());
            let _ = GlobalUnlock(memory);
            if let Err(error) = EmptyClipboard() {
                let _ = GlobalFree(Some(memory));
                return Err(io("emptying the clipboard", &error));
            }
            if let Err(error) =
                SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(memory.0)))
            {
                let _ = GlobalFree(Some(memory));
                return Err(io("writing the clipboard", &error));
            }
        }
        Ok(())
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            backend: "windows".to_owned(),
            input: true,
            clipboard: true,
            lock_detection: true,
        }
    }
}

fn io(what: &str, error: &windows::core::Error) -> GuiError {
    GuiError::io(format!("{what}: {error}"))
}

/// The clipboard, open for the calling thread until dropped.
struct Clipboard;

impl Clipboard {
    /// Opens it, trying again for up to `wait` while another program holds it.
    fn open(wait: Duration) -> Result<Self, GuiError> {
        let start = Instant::now();
        loop {
            // SAFETY: a plain call; no window owns the clipboard while it is open.
            match unsafe { OpenClipboard(None) } {
                Ok(()) => return Ok(Self),
                Err(error) if start.elapsed() >= wait => {
                    return Err(GuiError::unavailable(format!(
                        "another program holds the clipboard: {error}"
                    )));
                }
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // SAFETY: the clipboard was opened by `open` on this thread.
        let _ = unsafe { CloseClipboard() };
    }
}

/// Sends `ops` in order, as one batch, and says how many were taken.
fn send(ops: &[Op]) -> usize {
    let inputs: Vec<INPUT> = ops.iter().map(to_input).collect();
    // SAFETY: `inputs` holds `INPUT` structures of the size passed.
    let sent = unsafe { SendInput(&inputs, size_of::<INPUT>() as i32) };
    sent as usize
}

fn to_input(op: &Op) -> INPUT {
    match *op {
        Op::Mouse { x, y, data, flags } => INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: x,
                    dy: y,
                    mouseData: data,
                    dwFlags: MOUSE_EVENT_FLAGS(flags),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        Op::Key { vk, unit, flags } => INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: unit,
                    dwFlags: KEYBD_EVENT_FLAGS(flags),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    }
}

fn describe(monitor: HMONITOR) -> Option<Monitor> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
    // SAFETY: `info` is a `MONITORINFOEXW`, whose first member is the `MONITORINFO` the call takes, with its size set.
    let found =
        unsafe { GetMonitorInfoW(monitor, std::ptr::addr_of_mut!(info).cast::<MONITORINFO>()) };
    if !found.as_bool() {
        return None;
    }
    let rect = info.monitorInfo.rcMonitor;
    let device = wide_to_string(&info.szDevice);
    let (mut dpi_x, mut dpi_y) = (96_u32, 96_u32);
    // SAFETY: the out pointers are locals.
    let _ = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    let rotation = rotation_of(&info.szDevice);
    let id = device.trim_start_matches(r"\\.\").to_ascii_lowercase();
    Some(Monitor {
        display: Display {
            id: DisplayId(id),
            name: friendly_name(&info.szDevice),
            primary: info.monitorInfo.dwFlags & 1 != 0,
            x: rect.left,
            y: rect.top,
            width: u32::try_from(rect.right - rect.left).ok()?,
            height: u32::try_from(rect.bottom - rect.top).ok()?,
            scale: f64::from(dpi_x) / 96.0,
            rotation,
        },
        device,
        rotated: rotation.is_some_and(|degrees| degrees != 0),
    })
}

/// The display's rotation in degrees, when the driver says.
fn rotation_of(device: &[u16; 32]) -> Option<u32> {
    let mut mode = DEVMODEW {
        dmSize: size_of::<DEVMODEW>() as u16,
        ..DEVMODEW::default()
    };
    // SAFETY: `device` is NUL terminated and `mode` has its size set.
    let found = unsafe {
        EnumDisplaySettingsW(
            PCWSTR(device.as_ptr()),
            windows::Win32::Graphics::Gdi::ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
    };
    if !found.as_bool() {
        return None;
    }
    // SAFETY: the orientation lives in the display arm of the union, which is the arm `EnumDisplaySettingsW` fills for a
    // display (the printer arm shares its bytes).
    let orientation = unsafe { mode.Anonymous1.Anonymous2.dmDisplayOrientation };
    Some(orientation.0 * 90)
}

/// The monitor's name for a person, when Windows has one.
fn friendly_name(device: &[u16; 32]) -> Option<String> {
    let mut found = DISPLAY_DEVICEW {
        cb: size_of::<DISPLAY_DEVICEW>() as u32,
        ..DISPLAY_DEVICEW::default()
    };
    // SAFETY: `device` is NUL terminated and `found` has its size set.
    let listed = unsafe { EnumDisplayDevicesW(PCWSTR(device.as_ptr()), 0, &mut found, 0) };
    listed
        .as_bool()
        .then(|| wide_to_string(&found.DeviceString))
        .filter(|name| !name.is_empty())
}

fn wide_to_string(text: &[u16]) -> String {
    let end = text
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(text.len());
    String::from_utf16_lossy(&text[..end])
}

/// The session's connect state, when the terminal service answers.
fn connection_of(session: u32) -> Option<Connection> {
    let mut buffer = PWSTR::null();
    let mut length = 0_u32;
    // SAFETY: the out pointers are locals; a successful call hands over a buffer that is freed below.
    let queried = unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            session,
            WTSConnectState,
            &mut buffer,
            &mut length,
        )
    };
    queried.ok()?;
    // SAFETY: for `WTSConnectState` the buffer holds one `WTS_CONNECTSTATE_CLASS` (an `i32`), `length` bytes of it.
    let state = unsafe {
        let state =
            (length as usize >= size_of::<i32>()).then(|| buffer.0.cast::<i32>().read_unaligned());
        WTSFreeMemory(buffer.0.cast());
        state
    }?;
    Some(if state == WTSActive.0 {
        Connection::Active
    } else {
        Connection::Away
    })
}

/// The name of the account the session belongs to.
fn user_of(session: u32) -> Option<String> {
    let mut buffer = PWSTR::null();
    let mut length = 0_u32;
    // SAFETY: the out pointers are locals; a successful call hands over a NUL-terminated string freed below.
    unsafe {
        WTSQuerySessionInformationW(
            Some(WTS_CURRENT_SERVER_HANDLE),
            session,
            WTSUserName,
            &mut buffer,
            &mut length,
        )
        .ok()?;
        let name = buffer.to_string().ok();
        WTSFreeMemory(buffer.0.cast());
        name.filter(|name| !name.is_empty())
    }
}

/// The name of the desktop that has the input (`Default` for a person's own), or `None` when it cannot be opened, as it
/// cannot from a user session while the secure desktop has the screen.
fn input_desktop_name() -> Option<String> {
    // SAFETY: the desktop handle is made here and closed before returning; the buffer is a local of the length passed.
    unsafe {
        let desktop = OpenInputDesktop(
            DESKTOP_CONTROL_FLAGS(0),
            false,
            DESKTOP_ACCESS_FLAGS(0x0001),
        )
        .ok()?;
        let mut name = [0_u16; 256];
        let mut needed = 0_u32;
        let read = GetUserObjectInformationW(
            HANDLE(desktop.0),
            UOI_NAME,
            Some(name.as_mut_ptr().cast()),
            size_of_val(&name) as u32,
            Some(&mut needed),
        );
        let _ = CloseDesktop(desktop);
        read.ok()?;
        Some(wide_to_string(&name))
    }
}
