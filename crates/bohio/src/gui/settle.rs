//! Waiting for a screen to stop changing: a capture every interval, compared tile by tile, so one round trip replaces a
//! client's polling.

use std::thread;
use std::time::{Duration, Instant};

use xxhash_rust::xxh3::Xxh3;

use super::desktop::{Frame, GuiError};

/// The side of the tiles a frame is compared in.
const TILE: usize = 64;

/// How a settle runs.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Params {
    /// Time between captures.
    pub(crate) interval: Duration,
    /// How long the screen must stay unchanged.
    pub(crate) quiet: Duration,
    /// How long to wait at most.
    pub(crate) timeout: Duration,
    /// Tiles that may change between two captures without counting.
    pub(crate) ignore_tiles: u32,
}

/// How a settle ended, with the last frame it captured.
#[derive(Debug)]
pub(crate) struct Outcome {
    pub(crate) settled: bool,
    pub(crate) waited: Duration,
    pub(crate) frame: Frame,
}

/// An xxh3 of each 64 by 64 tile of `frame`, row by row.
pub(crate) fn tiles(frame: &Frame) -> Vec<u64> {
    let width = frame.width() as usize;
    let height = frame.height() as usize;
    let across = width.div_ceil(TILE);
    let down = height.div_ceil(TILE);
    let rgb = frame.rgb();
    let mut hashes = Vec::with_capacity(across * down);
    for tile_y in 0..down {
        for tile_x in 0..across {
            let mut hasher = Xxh3::new();
            let left = tile_x * TILE;
            let right = (left + TILE).min(width);
            for y in tile_y * TILE..((tile_y + 1) * TILE).min(height) {
                hasher.update(&rgb[(y * width + left) * 3..(y * width + right) * 3]);
            }
            hashes.push(hasher.digest());
        }
    }
    hashes
}

/// How many tiles differ; every tile when the frames are not the same size.
pub(crate) fn changed(before: &[u64], after: &[u64]) -> usize {
    if before.len() == after.len() {
        before.iter().zip(after).filter(|(a, b)| a != b).count()
    } else {
        after.len().max(before.len())
    }
}

/// Captures until no more than `ignore_tiles` tiles changed between captures for `quiet`, or `timeout` passes. The first
/// capture is the baseline, so the quiet time is counted from it. `capture` is the desktop's; `aborted` is asked between
/// captures so a stream that closed stops the wait.
pub(crate) fn settle(
    capture: &mut dyn FnMut() -> Result<Frame, GuiError>,
    params: &Params,
    aborted: &dyn Fn() -> bool,
) -> Result<Outcome, GuiError> {
    let started = Instant::now();
    let mut frame = capture()?;
    let mut previous = tiles(&frame);
    let mut quiet_since = Instant::now();
    loop {
        let now = Instant::now();
        let waited = now.duration_since(started);
        if now.duration_since(quiet_since) >= params.quiet {
            return Ok(Outcome {
                settled: true,
                waited,
                frame,
            });
        }
        if waited >= params.timeout || aborted() {
            return Ok(Outcome {
                settled: false,
                waited,
                frame,
            });
        }
        let until_quiet = params.quiet.saturating_sub(now.duration_since(quiet_since));
        let until_timeout = params.timeout.saturating_sub(waited);
        thread::sleep(params.interval.min(until_quiet).min(until_timeout));
        frame = capture()?;
        let current = tiles(&frame);
        if changed(&previous, &current) > params.ignore_tiles as usize {
            quiet_since = Instant::now();
        }
        previous = current;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, shade: u8) -> Frame {
        Frame::new(
            width,
            height,
            vec![shade; width as usize * height as usize * 3],
        )
        .unwrap()
    }

    #[test]
    fn tiles_report_exactly_the_tiles_that_changed() {
        let before = frame(130, 70, 0);
        let mut pixels = before.rgb().to_vec();
        // One pixel in the second tile of the first row of tiles.
        pixels[(3 * 130 + 70) * 3] = 9;
        let after = Frame::new(130, 70, pixels).unwrap();
        assert_eq!(tiles(&before).len(), 6);
        assert_eq!(changed(&tiles(&before), &tiles(&after)), 1);
        assert_eq!(changed(&tiles(&before), &tiles(&frame(64, 64, 0))), 6);
    }

    #[test]
    fn a_screen_that_never_stops_changing_times_out_and_one_that_stops_settles() {
        let params = Params {
            interval: Duration::from_millis(5),
            quiet: Duration::from_millis(40),
            timeout: Duration::from_millis(200),
            ignore_tiles: 0,
        };
        let mut shade = 0_u8;
        let mut moving = || {
            shade = shade.wrapping_add(1);
            Ok(frame(64, 64, shade))
        };
        let outcome = settle(&mut moving, &params, &|| false).unwrap();
        assert!(!outcome.settled);
        assert!(outcome.waited >= Duration::from_millis(200));
        let mut still = || Ok(frame(64, 64, 7));
        let outcome = settle(&mut still, &params, &|| false).unwrap();
        assert!(outcome.settled);
    }
}
