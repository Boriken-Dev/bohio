//! A desktop that exists only in memory and moves by itself, for measuring a watch: one display of a chosen size on which a
//! scene plays. `text` types into a page and scrolls it now and then; `motion` moves a block of photographic detail across a
//! smooth background. Nothing here touches a screen.
#![allow(dead_code, reason = "each includer uses some of it")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::gui::{Capabilities, Desktop, DesktopState, Frame, GuiError};
use bohio_proto::gui::{Display, DisplayId, InputEvent, Rect};

/// What plays on the display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scene {
    /// Typing into a page and scrolling it.
    Text,
    /// A block of photographic detail moving.
    Motion,
}

impl Scene {
    /// The scene named `name`: `text` or `motion`.
    pub fn named(name: &str) -> Option<Self> {
        match name {
            "text" => Some(Self::Text),
            "motion" => Some(Self::Motion),
            _ => None,
        }
    }
}

struct Screen {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
}

/// The synthetic desktop: a scene thread changes the pixels thirty times a second until it is dropped.
pub struct Synthetic {
    screen: Arc<Mutex<Screen>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

fn hash(a: u32, b: u32) -> u32 {
    let mut h = a.wrapping_mul(0x9e37_79b1) ^ b.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2c1b_3c6d);
    h ^ (h >> 12)
}

/// A pixel of the smooth background.
fn background(x: u32, y: u32, width: u32, height: u32) -> [u8; 3] {
    let u = x as f32 / width as f32;
    let v = y as f32 / height as f32;
    [
        (40.0 + 120.0 * u) as u8,
        (70.0 + 100.0 * v) as u8,
        (150.0 - 60.0 * u * v) as u8,
    ]
}

/// A pixel of the photographic block: waves and grain, so a lossless coder gets little from it.
fn photo(x: u32, y: u32, phase: f32) -> [u8; 3] {
    let (fx, fy) = (x as f32, y as f32);
    let base = 128.0 + 60.0 * (fx / 23.0 + phase).sin() + 40.0 * (fy / 17.0 - phase).sin();
    let grain = (hash(x, y) % 25) as f32 - 12.0;
    [
        (base + grain).clamp(0.0, 255.0) as u8,
        (base * 0.8 + 30.0 + grain).clamp(0.0, 255.0) as u8,
        (255.0 - base * 0.7 + grain).clamp(0.0, 255.0) as u8,
    ]
}

impl Synthetic {
    /// A display of `width` by `height` playing `scene`.
    pub fn new(width: u32, height: u32, scene: Scene) -> Self {
        let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
        for y in 0..height {
            for x in 0..width {
                let color = match scene {
                    Scene::Text => [244, 244, 240],
                    Scene::Motion => background(x, y, width, height),
                };
                rgb.extend_from_slice(&color);
            }
        }
        let screen = Arc::new(Mutex::new(Screen { width, height, rgb }));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = thread::spawn({
            let (screen, stop) = (screen.clone(), stop.clone());
            move || play(&screen, &stop, scene)
        });
        Self {
            screen,
            stop,
            thread: Some(thread),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Screen> {
        self.screen.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Synthetic {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn play(screen: &Mutex<Screen>, stop: &AtomicBool, scene: Scene) {
    let mut step = 0_u32;
    let started = Instant::now();
    while !stop.load(Ordering::SeqCst) {
        {
            let mut screen = screen.lock().unwrap_or_else(PoisonError::into_inner);
            match scene {
                Scene::Text => type_one(&mut screen, step),
                Scene::Motion => move_block(&mut screen, step),
            }
        }
        step += 1;
        let due = started + Duration::from_millis(u64::from(step) * 33);
        thread::sleep(due.saturating_duration_since(Instant::now()));
    }
}

const GLYPH_W: u32 = 8;
const GLYPH_H: u32 = 14;
const MARGIN: u32 = 40;

/// Types the `step`th character of a page that wraps at the display's width, and scrolls the page one line when it fills.
fn type_one(screen: &mut Screen, step: u32) {
    let columns = (screen.width - 2 * MARGIN) / GLYPH_W;
    let rows = (screen.height - 2 * MARGIN) / GLYPH_H;
    // One character in three is a space; a character takes a new place on every step.
    let (index, wrap) = (step % (columns * rows), step / (columns * rows));
    let (column, row) = (index % columns, index / columns);
    if index == 0 && wrap > 0 {
        let blank = [244_u8, 244, 240];
        for y in MARGIN..screen.height - MARGIN {
            for x in MARGIN..screen.width - MARGIN {
                let at = ((y * screen.width + x) * 3) as usize;
                screen.rgb[at..at + 3].copy_from_slice(&blank);
            }
        }
    }
    // The page scrolls one line every sixty characters once it has a few lines.
    if step % 60 == 59 && row >= 3 {
        scroll(screen, rows);
    }
    let draw_row = if step % 60 == 59 && row >= 3 {
        row - 1
    } else {
        row
    };
    if hash(step, 7).is_multiple_of(3) {
        return;
    }
    let (left, top) = (MARGIN + column * GLYPH_W, MARGIN + draw_row * GLYPH_H);
    for gy in 0..9 {
        for gx in 0..5 {
            if hash(step % 61 + 1, gy * 5 + gx) % 5 < 2 {
                for (dx, dy) in [(0, 0), (1, 0)] {
                    let (x, y) = (left + gx + dx, top + 2 + gy + dy);
                    let at = ((y * screen.width + x) * 3) as usize;
                    screen.rgb[at..at + 3].copy_from_slice(&[20, 20, 24]);
                }
            }
        }
    }
}

/// Moves the page up by one line of text.
fn scroll(screen: &mut Screen, rows: u32) {
    let line = (GLYPH_H * screen.width * 3) as usize;
    let top = (MARGIN * screen.width * 3) as usize;
    let page = (rows * GLYPH_H * screen.width * 3) as usize;
    screen.rgb.copy_within(top + line..top + page, top);
    let blank = [244_u8, 244, 240];
    let from = top + page - line;
    for at in (from..from + line).step_by(3) {
        screen.rgb[at..at + 3].copy_from_slice(&blank);
    }
}

/// Paints the block `step` steps along its path, restoring the background where it was.
fn move_block(screen: &mut Screen, step: u32) {
    let (width, height) = (screen.width, screen.height);
    let (block_w, block_h) = (width / 4, height / 4);
    let travel_x = width - block_w;
    let travel_y = height - block_h;
    let position = |n: u32| {
        let leg = |n: u32, travel: u32| {
            let along = (n * 9) % (2 * travel.max(1));
            if along < travel {
                along
            } else {
                2 * travel - along
            }
        };
        (leg(n, travel_x), leg(n * 2 / 3, travel_y))
    };
    let (old_x, old_y) = position(step.saturating_sub(1));
    for y in old_y..old_y + block_h {
        for x in old_x..old_x + block_w {
            let at = ((y * width + x) * 3) as usize;
            screen.rgb[at..at + 3].copy_from_slice(&background(x, y, width, height));
        }
    }
    let (x0, y0) = position(step);
    let phase = step as f32 / 6.0;
    for y in y0..y0 + block_h {
        for x in x0..x0 + block_w {
            let at = ((y * width + x) * 3) as usize;
            screen.rgb[at..at + 3].copy_from_slice(&photo(x - x0, y - y0, phase));
        }
    }
}

impl Desktop for Synthetic {
    fn state(&self) -> Result<DesktopState, GuiError> {
        let screen = self.lock();
        Ok(DesktopState {
            user: Some("synthetic".to_owned()),
            active: true,
            locked: Some(false),
            displays: vec![Display {
                id: DisplayId::from("0"),
                name: Some("synthetic".to_owned()),
                primary: true,
                x: 0,
                y: 0,
                width: screen.width,
                height: screen.height,
                scale: 1.0,
                rotation: Some(0),
            }],
        })
    }

    fn capture(&self, _: &DisplayId, region: Option<Rect>) -> Result<Frame, GuiError> {
        let screen = self.lock();
        let rect = region.unwrap_or(Rect::new(0, 0, screen.width, screen.height));
        let mut rgb = Vec::with_capacity((rect.width * rect.height * 3) as usize);
        for y in rect.y..rect.y + rect.height {
            let from = ((y * screen.width + rect.x) * 3) as usize;
            rgb.extend_from_slice(&screen.rgb[from..from + (rect.width * 3) as usize]);
        }
        Frame::new(rect.width, rect.height, rgb)
    }

    fn input(&self, events: &[InputEvent]) -> Result<u32, GuiError> {
        Ok(u32::try_from(events.len()).unwrap_or(u32::MAX))
    }

    fn cursor(&self) -> Option<(DisplayId, i32, i32)> {
        None
    }

    fn clipboard_text(&self) -> Result<String, GuiError> {
        Err(GuiError::unsupported(
            "the synthetic desktop has no clipboard",
        ))
    }

    fn set_clipboard_text(&self, _: &str) -> Result<(), GuiError> {
        Err(GuiError::unsupported(
            "the synthetic desktop has no clipboard",
        ))
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            backend: "synthetic".to_owned(),
            input: false,
            clipboard: false,
            lock_detection: false,
        }
    }
}
