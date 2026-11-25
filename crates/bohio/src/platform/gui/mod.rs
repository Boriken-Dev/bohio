//! Desktop backends: what a node serves of the screen it runs in.
//!
//! `input` and `session` hold the rules that need no operating system (the keys and the pointer's coordinates, what a
//! session's answers mean, the clipboard's text) and are tested on every platform. `windows` and `windows_capture` are
//! the Windows backend, the only files here that hold `unsafe`: each call that has no safe binding is commented where
//! it is made, and `tests/unsafe_files.rs` says why it was needed. The X11 backend (the `bohio-x11` crate) is the Linux
//! backend over an X display, in safe Rust; `wayland` (feature `gui-wayland`) is the one over the desktop portals and
//! PipeWire, also safe. `mac_rules` holds the macOS backend's rules (tested everywhere) and `macos`
//! the calls to ScreenCaptureKit and Core Graphics, the third file here that holds `unsafe`.

#![cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the rules are the Windows backend's and are tested on every platform"
    )
)]

mod input;
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "the rules are the macOS backend's and are tested on every platform"
    )
)]
mod mac_rules;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "linux")]
mod own;
mod session;
#[cfg(all(target_os = "linux", feature = "gui-wayland"))]
mod wayland;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_capture;

#[cfg(target_os = "macos")]
pub use self::macos::{MacosDesktop, MacosSettings};
#[cfg(target_os = "linux")]
pub use self::own::{
    OwnSession, check_runtime_dir, display as own_display, own_uid, runtime_dir as own_runtime_dir,
};
#[cfg(all(target_os = "linux", feature = "gui-wayland"))]
pub use self::wayland::{Persist, WaylandDesktop, WaylandSettings};
#[cfg(windows)]
pub use self::windows::{WindowsDesktop, WindowsSettings};
