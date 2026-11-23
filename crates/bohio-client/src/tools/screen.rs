//! The desktop: a snapshot, input, and a short watch, for a peer the host lets look at or drive it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bohio_proto::gui::{DisplayId, GuiRequest, ImageFormat, InputEvent, Rect, SettleOptions, Want};
use serde::Deserialize;
use serde_json::{Value, json};

use super::{Cancel, Image, Output, ToolError, Tools, lock, parse};
use crate::Error;
use crate::gui::{Gui, SnapshotOptions, WatchEvent, WatchOptions};

/// The desktop stream kept between calls.
pub(crate) struct Held {
    gui: Arc<Gui>,
    wants: Vec<Want>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Settle {
    Flag(bool),
    Options(SettleOptions),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotArgs {
    display: Option<DisplayId>,
    region: Option<Rect>,
    format: Option<ImageFormat>,
    quality: Option<u8>,
    max_width: Option<u32>,
    max_height: Option<u32>,
    #[serde(default)]
    cursor: bool,
    settle: Option<Settle>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputArgs {
    #[serde(default)]
    events: Vec<InputEvent>,
    keys: Option<String>,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchArgs {
    duration_ms: Option<u64>,
    display: Option<DisplayId>,
    region: Option<Rect>,
    fps: Option<u32>,
    max_width: Option<u32>,
    max_height: Option<u32>,
    #[serde(default)]
    cursor: bool,
}

fn mime(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Webp => "image/webp",
        _ => "image/png",
    }
}

fn lost(error: &Error) -> bool {
    matches!(error, Error::Closed | Error::Transport(_))
}

impl Tools {
    /// The open desktop stream, holding at least `need`; one is opened when none is, or when it lacks a want.
    fn desktop(&self, need: &[Want]) -> Result<Arc<Gui>, ToolError> {
        let mut held = lock(&self.desktop);
        if let Some(open) = held.as_ref()
            && need.iter().all(|want| open.wants.contains(want))
        {
            return Ok(Arc::clone(&open.gui));
        }
        let mut wants = held
            .as_ref()
            .map(|open| open.wants.clone())
            .unwrap_or_default();
        for want in need {
            if !wants.contains(want) {
                wants.push(*want);
            }
        }
        let opened = Gui::open_within(
            self.link(),
            &GuiRequest::new(wants.clone()),
            Some(self.settings.gui_timeout),
        );
        // A kept stream that the grant does not widen stays: only the new want is refused.
        let gui = Arc::new(opened?);
        *held = Some(Held {
            gui: Arc::clone(&gui),
            wants,
        });
        Ok(gui)
    }

    fn forget_desktop(&self) {
        *lock(&self.desktop) = None;
    }
}

pub(super) fn snapshot(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: SnapshotArgs = parse(arguments)?;
    let options = SnapshotOptions {
        display: args.display,
        region: args.region,
        format: args.format,
        quality: args.quality,
        max_width: args.max_width,
        max_height: args.max_height,
        cursor: args.cursor,
        settle: match args.settle {
            Some(Settle::Options(options)) => Some(options),
            Some(Settle::Flag(true)) => Some(SettleOptions::default()),
            _ => None,
        },
    };
    let wait = Some(tools.settings.gui_timeout);
    let mut tried = 0;
    let image = loop {
        let gui = tools.desktop(&[Want::View])?;
        match gui.snapshot(&options, wait) {
            Ok(image) => break image,
            // A snapshot changes nothing, so a stream that was lost is opened again once.
            Err(error) if lost(&error) && tried == 0 => {
                tried += 1;
                tools.forget_desktop();
            }
            Err(error) => return Err(error.into()),
        }
    };
    if image.bytes.len() as u64 > tools.settings.gui_max_image {
        return Err(ToolError::new(
            "too_large",
            format!(
                "the image is {} bytes, more than the {} a call returns: ask for a smaller `max_width` and `max_height`, a `region`, or `jpeg`",
                image.bytes.len(),
                tools.settings.gui_max_image
            ),
        ));
    }
    let header = serde_json::to_value(&image.header).unwrap_or(Value::Null);
    Ok(Output {
        value: json!({"image": header, "bytes": image.bytes.len()}),
        images: vec![Image {
            mime: mime(image.header.format).to_owned(),
            data: image.bytes,
        }],
    })
}

pub(super) fn input(
    tools: &Tools,
    arguments: Value,
    _cancel: &Cancel,
) -> Result<Output, ToolError> {
    let args: InputArgs = parse(arguments)?;
    let mut events = args.events;
    if let Some(combo) = &args.keys {
        events.extend(
            InputEvent::combo(combo).map_err(|error| ToolError::bad(format!("`keys`: {error}")))?,
        );
    }
    if let Some(text) = args.text {
        events.push(InputEvent::Text { text });
    }
    if events.is_empty() {
        return Err(ToolError::bad("give `events`, `keys` or `text`"));
    }
    let gui = tools.desktop(&[Want::Input])?;
    // Input is never sent again: a stream lost after it was written may have applied it.
    let applied = gui
        .input(&events, Some(tools.settings.gui_timeout))
        .inspect_err(|error| {
            if lost(error) {
                tools.forget_desktop();
            }
        })?;
    Output::json(&json!({"applied": applied, "sent": events.len()}))
}

pub(super) fn watch(tools: &Tools, arguments: Value, cancel: &Cancel) -> Result<Output, ToolError> {
    let args: WatchArgs = parse(arguments)?;
    let length =
        Duration::from_millis(args.duration_ms.unwrap_or(2000)).min(tools.settings.gui_timeout);
    let gui = tools.desktop(&[Want::View])?;
    gui.watch(&WatchOptions {
        display: args.display,
        region: args.region,
        fps: args.fps,
        max_width: args.max_width,
        max_height: args.max_height,
        cursor: args.cursor,
        ..WatchOptions::default()
    })?;
    let begun = Instant::now();
    let (mut updates, mut rects, mut bytes, mut cursors) = (0_u64, 0_u64, 0_u64, 0_u64);
    let mut size = None;
    let result = loop {
        if cancel.is_cancelled() || begun.elapsed() >= length {
            break Ok(());
        }
        match gui.next_event(Some(tools.settings.poll)) {
            Ok(WatchEvent::Update(update)) => {
                updates += 1;
                rects += update.header.rects.len() as u64;
                bytes += update.bytes;
                size = Some((update.header.width, update.header.height));
            }
            Ok(WatchEvent::Cursor(_)) => cursors += 1,
            Err(Error::Timeout) => {}
            Err(error) => break Err(error),
        }
    };
    let _ = gui.unwatch();
    if let Err(error) = result {
        if lost(&error) {
            tools.forget_desktop();
        }
        return Err(error.into());
    }
    Output::json(&json!({
        "updates": updates,
        "rectangles": rects,
        "update_bytes": bytes,
        "cursor_moves": cursors,
        "width": size.map(|(width, _)| width),
        "height": size.map(|(_, height)| height),
        "duration_ms": u64::try_from(begun.elapsed().as_millis()).unwrap_or(u64::MAX),
    }))
}
