// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A session of a desktop source, as the engine drives it: how many questions a capture asks the desktop, and that a
//! frame the provider sizes larger than the region asked is refused before anything is allocated for it.

#![forbid(unsafe_code)]

#[path = "../examples/support/painted.rs"]
mod painted;

use std::sync::Arc;

use bohio::access::{DesktopOf, KindDesktop, Sources};
use bohio::gui::{Desktop, DesktopState, Frame, GuiError};
use bohio_proto::ErrorCode;
use bohio_proto::gui::{Display, DisplayId, InputEvent, Rect};
use painted::Painted;

fn session(desktop: Arc<dyn Desktop>) -> KindDesktop {
    let sources = Sources::default().with_desktop("test", Arc::new(DesktopOf::new(desktop)));
    KindDesktop::open(sources.desktop(Some("test")).expect("the source")).expect("a session")
}

#[test]
fn a_capture_asks_the_desktop_for_its_state_at_most_once() {
    let painted = Arc::new(Painted::new());
    let desktop = session(Arc::clone(&painted) as Arc<dyn Desktop>).bounded(1_000_000);
    let display = DisplayId("0".to_owned());
    let before = painted.calls();
    desktop.capture(&display, None).expect("a whole display");
    // One question for the display's size, one for the picture.
    assert_eq!(painted.calls() - before, 2);
    let before = painted.calls();
    desktop
        .capture(&display, Some(Rect::new(0, 0, 10, 10)))
        .expect("a region");
    // A region carries its own size: the picture alone.
    assert_eq!(painted.calls() - before, 1);
}

/// A desktop whose display is far larger than any host captures; it can make no picture.
struct Vast;

impl Desktop for Vast {
    fn state(&self) -> Result<DesktopState, GuiError> {
        Ok(DesktopState::new(
            true,
            Some(false),
            vec![Display {
                id: DisplayId("0".to_owned()),
                name: None,
                primary: true,
                x: 0,
                y: 0,
                width: 20_000,
                height: 20_000,
                scale: 1.0,
                rotation: Some(0),
            }],
        ))
    }

    fn capture(&self, _: &DisplayId, _: Option<Rect>) -> Result<Frame, GuiError> {
        Err(GuiError::new(ErrorCode::Internal, "no picture"))
    }

    fn input(&self, _: &[InputEvent]) -> Result<u32, GuiError> {
        Ok(0)
    }

    fn cursor(&self) -> Option<(DisplayId, i32, i32)> {
        None
    }

    fn clipboard_text(&self) -> Result<String, GuiError> {
        Ok(String::new())
    }

    fn set_clipboard_text(&self, _: &str) -> Result<(), GuiError> {
        Ok(())
    }
}

#[test]
fn a_frame_larger_than_the_host_captures_is_refused_before_it_is_allocated() {
    let display = DisplayId("0".to_owned());
    let bounded = session(Arc::new(Vast)).bounded(1_000_000);
    let error = bounded.capture(&display, None).unwrap_err();
    assert!(error.message.contains("larger than"), "{}", error.message);
    // The region asked, not the display, sets the size.
    let error = bounded
        .capture(&display, Some(Rect::new(0, 0, 2_000, 2_000)))
        .unwrap_err();
    assert!(error.message.contains("larger than"), "{}", error.message);
    // A frame of the size asked is let through to the desktop, which here cannot make it.
    let error = bounded
        .capture(&display, Some(Rect::new(0, 0, 100, 100)))
        .unwrap_err();
    assert_eq!(error.message, "no picture");
    // A session no host bounded refuses a whole display and does not guess.
    let unbounded = session(Arc::new(Vast));
    assert!(unbounded.capture(&display, None).is_err());
}
