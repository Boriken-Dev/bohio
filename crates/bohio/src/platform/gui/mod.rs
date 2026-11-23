//! Desktop backends: what a node serves of the screen it runs in.
//!
//! `input` and `session` hold the rules that need no operating system (the keys and the pointer's coordinates, what a
//! session's answers mean, the clipboard's text) and are tested on every platform. `windows` and `windows_capture` are
//! the Windows backend, the only files here that hold `unsafe`: each call that has no safe binding is commented where
//! it is made, and `tests/unsafe_files.rs` says why it was needed.

#![cfg_attr(
    not(windows),
    allow(
        dead_code,
        reason = "the rules are the Windows backend's and are tested on every platform"
    )
)]

mod input;
mod session;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
mod windows_capture;

#[cfg(windows)]
pub use self::windows::{WindowsDesktop, WindowsSettings};
