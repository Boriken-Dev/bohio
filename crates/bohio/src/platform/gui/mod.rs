//! Desktop backends: what a node serves of the screen it runs in.
//!
//! `input` and `session` hold the rules that need no operating system (the keys and the pointer's coordinates, what a
//! session's answers mean, the clipboard's text) and are tested on every platform. `windows` and `windows_capture` are
//! the Windows backend, the only files here that hold `unsafe`: each call that has no safe binding is commented where
//! it is made, and `tests/unsafe_files.rs` says why it was needed. `x11`, `x11_clipboard` and `logind` are the Linux
//! backend over an X display, in safe Rust.

#![cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the rules are the Windows backend's and are tested on every platform"
    )
)]

mod input;
#[cfg(target_os = "linux")]
mod logind;
mod session;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_capture;
#[cfg(target_os = "linux")]
mod x11;
#[cfg(target_os = "linux")]
mod x11_clipboard;

#[cfg(windows)]
pub use self::windows::{WindowsDesktop, WindowsSettings};
#[cfg(target_os = "linux")]
pub use self::x11::{X11Desktop, X11Settings};
