//! A watch: the screen of a display sent as the tiles that changed, at a rate and quality the link keeps up with.
//!
//! The stream's thread captures only while fewer than `frames_in_flight` updates are unacknowledged, so nothing queues and
//! the delay is the link's. Each capture is compared tile by tile with the one before ([`TileGrid`]); the changed tiles
//! become rectangles, each encoded losslessly when that is cheap and as JPEG when it is not, and rectangles sent as JPEG
//! are resent lossless once they stay unchanged. The acknowledgements move the watch along the ladder in [`adapt`].

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use bohio_proto::gui::{
    Ack, CursorFrame, DisplayId, GuiOutput, Point, Rect, RectEncoding, UpdateHeader, UpdateRect,
    Watch, WatchEncoding,
};
use bohio_proto::{ErrorBody, ErrorCode};
use bohio_transfer::screen::{EncodeParams, Pixels, TileGrid, encode_rect, lossless};

use super::adapt::{Adapter, Pace};
use super::{Reply, Stream, Want, bad, encode};

/// One update the client has not yet acknowledged.
#[derive(Debug)]
struct Sent {
    frame: u64,
    at: Instant,
}

/// What a running watch holds.
pub(super) struct Watching {
    seq: u64,
    display: DisplayId,
    region: Option<Rect>,
    fps: u32,
    mode: WatchEncoding,
    quality: u8,
    max_width: Option<u32>,
    max_height: Option<u32>,
    cursor: bool,
    refine: bool,
    grid: TileGrid,
    /// The frame's size and scale as the client last heard them.
    shape: Option<(u32, u32, u64)>,
    key: bool,
    adapter: Adapter,
    pace: Pace,
    next_due: Instant,
    in_flight: VecDeque<Sent>,
    next_frame: u64,
    /// Tiles last sent as JPEG and unchanged since, with when they were sent.
    lossy: BTreeMap<usize, Instant>,
    last_cursor: Option<(DisplayId, i32, i32)>,
    checked_at: Instant,
}

impl Stream<'_> {
    /// Starts a watch, or replaces the settings of the one running.
    pub(super) fn watch(&mut self, request: &Watch) -> Reply {
        self.need(Want::View)?;
        let config = self.context.config;
        let state = self.state()?;
        let (display, region) = self.target(&state, request.display.as_ref(), request.region)?;
        if request.fps == Some(0) {
            return Err(bad("an update rate is at least 1 a second"));
        }
        if request.max_width == Some(0) || request.max_height == Some(0) {
            return Err(bad("a maximum size must not be zero"));
        }
        if request.max_rate == Some(0) {
            return Err(bad("a byte rate must not be zero"));
        }
        let quality = request.quality.unwrap_or(config.watch_quality);
        if !(1..=100).contains(&quality) {
            return Err(bad("the quality is from 1 to 100"));
        }
        let fps = request
            .fps
            .unwrap_or(config.default_fps)
            .min(config.max_fps)
            .max(1);
        let mode = request.encoding.unwrap_or_default();
        let refine = request.refine.unwrap_or(mode == WatchEncoding::Auto);
        let rate = match (request.max_rate, config.max_rate) {
            (Some(asked), Some(cap)) => Some(asked.min(cap)),
            (asked, cap) => asked.or(cap),
        };
        let now = Instant::now();
        let (in_flight, next_frame) = self
            .watch
            .take()
            .map_or((VecDeque::new(), 1), |old| (old.in_flight, old.next_frame));
        self.watch = Some(Watching {
            seq: request.seq,
            display: display.id,
            region: request.region.map(|_| region),
            fps,
            mode,
            quality,
            max_width: request.max_width,
            max_height: request.max_height,
            cursor: request.cursor,
            refine,
            grid: TileGrid::new(region.width, region.height, config.tile),
            shape: None,
            key: true,
            adapter: Adapter::new(config, fps, mode != WatchEncoding::Lossless),
            pace: Pace::new(rate, now),
            next_due: now,
            in_flight,
            next_frame,
            lossy: BTreeMap::new(),
            last_cursor: None,
            checked_at: now,
        });
        Ok(())
    }

    /// Stops the watch; what was sent may still arrive.
    pub(super) fn unwatch(&mut self) -> Reply {
        self.watch = None;
        Ok(())
    }

    /// Takes the client's acknowledgement of an update: it frees a place in flight and tells the ladder how late it was.
    pub(super) fn ack(&mut self, ack: &Ack) -> Reply {
        let Some(watch) = self.watch.as_mut() else {
            return Ok(());
        };
        let Some(at) = watch
            .in_flight
            .iter()
            .position(|sent| sent.frame == ack.frame)
        else {
            return Ok(());
        };
        let delay = watch.in_flight[at].at.elapsed();
        watch.in_flight.drain(..=at);
        watch.adapter.observe(delay);
        Ok(())
    }

    /// How long the stream's thread may wait for a request before the watch wants it again.
    pub(super) fn wake_in(&self, poll: Duration) -> Duration {
        match &self.watch {
            Some(watch) if self.has_room(watch) => {
                let at = watch.next_due.max(watch.pace.ready_at());
                poll.min(at.saturating_duration_since(Instant::now()))
            }
            _ => poll,
        }
    }

    fn has_room(&self, watch: &Watching) -> bool {
        watch.in_flight.len() < self.config().frames_in_flight as usize
    }

    /// Sends what changed since the last update, when one is due and the client has room for it. `false` when the stream
    /// ended.
    pub(super) fn tick(&mut self) -> bool {
        let Some(watch) = self.watch.as_ref() else {
            return true;
        };
        let now = Instant::now();
        if !self.has_room(watch) || now < watch.next_due.max(watch.pace.ready_at()) {
            return true;
        }
        match self.update(now) {
            Ok(alive) => alive,
            Err(body) => {
                let seq = self.watch.take().map_or(0, |watch| watch.seq);
                self.send(&GuiOutput::RequestError(
                    bohio_proto::gui::RequestError::new(seq, body),
                ))
            }
        }
    }

    /// One capture, its changes and their send.
    #[allow(
        clippy::too_many_lines,
        reason = "one pass: capture, fit, compare, encode, send"
    )]
    fn update(&mut self, now: Instant) -> Result<bool, ErrorBody> {
        let config = self.config();
        let state_interval = config.state_interval.0;
        let tile = config.tile;
        let refine_after = config.refine_after.0;
        let bpp = config.lossless_max_bpp;
        let zstd_level = config.zstd_level;
        let Some(watch) = self.watch.as_ref() else {
            return Ok(true);
        };
        if now.duration_since(watch.checked_at) >= state_interval {
            self.need(Want::View)?;
            if let Some(watch) = self.watch.as_mut() {
                watch.checked_at = now;
            }
        }
        let Some(watch) = self.watch.as_mut() else {
            return Ok(true);
        };
        let quality = watch.adapter.quality();
        let interval = Duration::from_secs_f64(1.0 / quality.fps.min(f64::from(watch.fps)));
        watch.next_due = now + interval;
        if !self.last_state.active {
            // The session is not at the screen: nothing to show until it returns.
            return Ok(true);
        }
        // A display that changed size since the last state poll is found by the capture: read the state again and take
        // the new size as the shape of the next frame. Only a display that still disagrees is an error.
        let mut refreshed = false;
        let (display_id, mode, region, captured) = loop {
            let Some(watch) = self.watch.as_ref() else {
                return Ok(true);
            };
            let display = self
                .last_state
                .displays
                .iter()
                .find(|display| display.id == watch.display)
                .ok_or_else(|| ErrorBody::new(ErrorCode::NotFound, "the display is gone"))?
                .clone();
            let region = watch
                .region
                .unwrap_or_else(|| Rect::new(0, 0, display.width, display.height));
            let (asked, mode) = (watch.region, watch.mode);
            let captured = if region.fits(display.width, display.height) {
                let desktop = self.desktop;
                let id = display.id.clone();
                Some(self.context.state.call(|| desktop.capture(&id, asked)))
            } else {
                None
            };
            match captured {
                Some(Ok(captured))
                    if (captured.width(), captured.height()) == (region.width, region.height) =>
                {
                    break (display.id, mode, region, captured);
                }
                _ if !refreshed => {
                    refreshed = true;
                    if !self.poll_state() {
                        return Ok(false);
                    }
                    if !self.last_state.active {
                        return Ok(true);
                    }
                }
                Some(Ok(_)) => {
                    return Err(ErrorBody::new(
                        ErrorCode::Internal,
                        "the desktop returned a picture of another size",
                    ));
                }
                Some(Err(error)) => return Err(error.into()),
                None => return Err(bad("the region is no longer inside the display")),
            }
        };
        let Some(watch) = self.watch.as_mut() else {
            return Ok(true);
        };
        let fit = encode::fitted(
            captured.width(),
            captured.height(),
            watch.max_width,
            watch.max_height,
        );
        let size = if quality.scale < 1.0 {
            let shrunk = |side: u32| ((f64::from(side) * quality.scale).round() as u32).max(1);
            (shrunk(fit.0), shrunk(fit.1))
        } else {
            fit
        };
        let frame = if size == (captured.width(), captured.height()) {
            captured
        } else {
            encode::resized(&captured, size)?
        };
        let shape = (size.0, size.1, region.width.into());
        let pixels = Pixels::new(size.0, size.1, frame.rgb()).map_err(|_| {
            ErrorBody::new(ErrorCode::Internal, "the picture does not match its size")
        })?;
        let key = watch.key || watch.shape != Some(shape);
        if watch.shape != Some(shape) || watch.grid.tile() != tile {
            watch.grid = TileGrid::new(size.0, size.1, tile);
            watch.lossy.clear();
        }
        if key {
            watch.grid.reset();
        }
        let changed = watch.grid.update(&pixels);
        let mut sending: Vec<(Rect, Option<RectEncoding>)> = Vec::new();
        if key {
            sending.push((Rect::new(0, 0, size.0, size.1), None));
            watch.lossy.clear();
        } else {
            for index in &changed {
                watch.lossy.remove(index);
            }
            sending.extend(
                watch
                    .grid
                    .rects(&changed)
                    .into_iter()
                    .map(|rect| (rect, None)),
            );
            if watch.refine {
                let due: Vec<usize> = watch
                    .lossy
                    .iter()
                    .filter(|(_, since)| now.duration_since(**since) >= refine_after)
                    .map(|(index, _)| *index)
                    .collect();
                for index in &due {
                    watch.lossy.remove(index);
                }
                sending.extend(
                    watch
                        .grid
                        .rects(&due)
                        .into_iter()
                        .map(|rect| (rect, Some(lossless()))),
                );
            }
        }
        let cursor_now = self.desktop.cursor();
        let watch = self.watch.as_mut().expect("a watch is running");
        if sending.is_empty() {
            return Ok(self.send_cursor_alone(&display_id, cursor_now));
        }
        // Encode each rectangle.
        let jpeg_quality = quality
            .jpeg
            .map_or(watch.quality, |cap| cap.min(watch.quality));
        let params = EncodeParams {
            quality: jpeg_quality,
            zstd_level,
        };
        let mut parts = Vec::with_capacity(sending.len());
        let mut bytes = Vec::new();
        let mut lossy_tiles = Vec::new();
        for (rect, forced) in &sending {
            let area = rect.area() as f64;
            let (encoding, encoded) = match (forced, mode) {
                (Some(encoding), _) => (*encoding, encode_one(&pixels, *rect, *encoding, &params)?),
                (None, WatchEncoding::Lossless) => {
                    (lossless(), encode_one(&pixels, *rect, lossless(), &params)?)
                }
                (None, WatchEncoding::Jpeg) => (
                    RectEncoding::Jpeg,
                    encode_one(&pixels, *rect, RectEncoding::Jpeg, &params)?,
                ),
                (None, WatchEncoding::Auto) => {
                    let exact = encode_one(&pixels, *rect, lossless(), &params)?;
                    if exact.len() as f64 <= bpp * area {
                        (lossless(), exact)
                    } else {
                        (
                            RectEncoding::Jpeg,
                            encode_one(&pixels, *rect, RectEncoding::Jpeg, &params)?,
                        )
                    }
                }
            };
            if encoding == RectEncoding::Jpeg {
                lossy_tiles.extend(watch.grid.tiles_in(*rect));
            }
            parts.push(UpdateRect {
                x: rect.x,
                y: rect.y,
                width: rect.width,
                height: rect.height,
                encoding,
                length: encoded.len() as u64,
            });
            bytes.extend_from_slice(&encoded);
        }
        if watch.refine {
            for index in lossy_tiles {
                watch.lossy.insert(index, now);
            }
        }
        let cursor = if watch.cursor {
            cursor_now
                .as_ref()
                .filter(|(on, _, _)| *on == display_id)
                .map(|(_, x, y)| Point { x: *x, y: *y })
        } else {
            None
        };
        let number = watch.next_frame;
        let header = UpdateHeader {
            frame: number,
            display: display_id,
            width: size.0,
            height: size.1,
            scale: f64::from(size.0) / f64::from(region.width),
            key,
            rects: parts,
            cursor,
            level: watch.adapter.level(),
            ack_ms: watch.adapter.ack_ms(),
        };
        watch.key = false;
        watch.shape = Some(shape);
        watch.next_frame += 1;
        watch.last_cursor = cursor_now;
        let sent_at = Instant::now();
        watch.in_flight.push_back(Sent {
            frame: number,
            at: sent_at,
        });
        let total = bytes.len() as u64;
        watch.pace.spent(total, sent_at);
        if !self.send(&GuiOutput::Update(header)) {
            return Ok(false);
        }
        for piece in bytes.chunks(self.chunk) {
            if !self.send(&GuiOutput::Data(piece.to_vec())) {
                return Ok(false);
            }
        }
        self.counts.frames += 1;
        Ok(true)
    }

    /// Tells the client where the pointer is when it moved and nothing else changed. `false` when the stream ended.
    fn send_cursor_alone(
        &mut self,
        display: &DisplayId,
        now: Option<(DisplayId, i32, i32)>,
    ) -> bool {
        let Some(watch) = self.watch.as_mut() else {
            return true;
        };
        if !watch.cursor || watch.last_cursor == now {
            return true;
        }
        watch.last_cursor.clone_from(&now);
        let frame = match now {
            Some((on, x, y)) => CursorFrame {
                visible: on == *display,
                display: on,
                x,
                y,
            },
            None => CursorFrame {
                display: display.clone(),
                x: 0,
                y: 0,
                visible: false,
            },
        };
        self.send(&GuiOutput::Cursor(frame))
    }
}

fn encode_one(
    pixels: &Pixels<'_>,
    rect: Rect,
    encoding: RectEncoding,
    params: &EncodeParams,
) -> Result<Vec<u8>, ErrorBody> {
    encode_rect(pixels, rect, encoding, params)
        .map_err(|_| ErrorBody::new(ErrorCode::Internal, "the rectangle cannot be encoded"))
}
