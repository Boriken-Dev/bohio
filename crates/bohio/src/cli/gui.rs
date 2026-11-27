//! `bohio remote ... gui` (look at and drive a host's desktop) and `bohio gui` (the owner answering a request to look).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use bohio_client::Open;
use bohio_client::gui::{Gui, Snapshot, Watch, WatchEvent};
use bohio_proto::gui::{
    Button, ButtonAction, DisplayId, GuiRequest, ImageFormat, InputEvent, Rect, SettleOptions,
    Want, WatchEncoding,
};
use clap::{Args, Subcommand};

use super::remote::{Client, control_endpoint, explain, unreachable};
use super::{ConfigOption, output};
use crate::config::Environment;

/// `bohio remote ... gui`.
#[derive(Debug, Args)]
pub struct RemoteGuiArgs {
    /// What to do with the host's desktop.
    #[command(subcommand)]
    pub action: GuiAction,
}

/// The `bohio remote ... gui` actions.
#[derive(Debug, Subcommand)]
pub enum GuiAction {
    /// Print what the host says of its desktop: backend, displays, state.
    Info,
    /// Capture a display, or a region of one, to a file or standard output.
    Snapshot(SnapshotArgs),
    /// Receive the screen as it changes and print what the stream costs.
    Watch(WatchArgs),
    /// Click at a point of a display.
    Click(ClickArgs),
    /// Move the pointer.
    Move(MoveArgs),
    /// Turn the mouse wheel, in notches (positive DY scrolls the content down).
    Scroll(ScrollArgs),
    /// Type text as it is, whatever the keyboard layout; `-` reads standard input.
    Type(TypeArgs),
    /// Press key combinations such as `ctrl+s`, `Return` or `alt+F4`, one after another.
    Key(KeyArgs),
    /// Read the desktop clipboard text, or replace it with `--set`.
    Clipboard(ClipboardArgs),
}

/// `gui snapshot`.
#[derive(Debug, Args)]
pub struct SnapshotArgs {
    /// The display; the primary one by default.
    #[arg(long, value_name = "ID")]
    pub display: Option<String>,
    /// A part of the display, `X,Y,W,H` in its pixels.
    #[arg(long, value_name = "X,Y,W,H")]
    pub region: Option<String>,
    /// png, jpeg or webp; the host's first allowed one by default.
    #[arg(long = "image-format", value_name = "FORMAT")]
    pub format: Option<String>,
    /// JPEG quality, 1 to 100.
    #[arg(long, value_name = "N")]
    pub quality: Option<u8>,
    /// Scale down to fit `WxH`; never up.
    #[arg(long, value_name = "WxH")]
    pub max_size: Option<String>,
    /// Wait for the screen to stop changing first.
    #[arg(long)]
    pub settle: bool,
    /// Where to write the image; `-` is standard output.
    #[arg(short, long, value_name = "FILE")]
    pub out: PathBuf,
}

/// `gui watch`.
#[derive(Debug, Args)]
pub struct WatchArgs {
    /// The display; the primary one by default.
    #[arg(long, value_name = "ID")]
    pub display: Option<String>,
    /// A part of the display, `X,Y,W,H` in its pixels.
    #[arg(long, value_name = "X,Y,W,H")]
    pub region: Option<String>,
    /// Updates a second at most; the host default when absent.
    #[arg(long, value_name = "N")]
    pub fps: Option<u32>,
    /// auto, lossless or jpeg; the host chooses per rectangle (auto) by default.
    #[arg(long, value_name = "ENCODING")]
    pub encoding: Option<String>,
    /// The JPEG quality a rectangle starts at, 1 to 100.
    #[arg(long, value_name = "N")]
    pub quality: Option<u8>,
    /// Scale the frame down to fit `WxH`; never up.
    #[arg(long, value_name = "WxH")]
    pub max_size: Option<String>,
    /// The most bytes a second the updates may add up to.
    #[arg(long, value_name = "BYTES")]
    pub max_rate: Option<u64>,
    /// Stop after this many seconds; without it the watch runs until the host ends it.
    #[arg(long, value_name = "N")]
    pub seconds: Option<u64>,
    /// Print a line a second, and a total at the end: updates, bytes, the host ack delay and its ladder level.
    #[arg(long)]
    pub stats: bool,
    /// Write each composed frame to DIR as a PNG, for diagnosis. The files hold the screen: keep them private.
    #[arg(long, value_name = "DIR")]
    pub out: Option<PathBuf>,
}

/// `gui click`.
#[derive(Debug, Args)]
pub struct ClickArgs {
    /// Pixels from the display's left edge.
    pub x: i32,
    /// Pixels from the display's top edge.
    pub y: i32,
    /// left, right, middle, back or forward.
    #[arg(long, default_value = "left")]
    pub button: String,
    /// How many clicks (2 is a double click).
    #[arg(long, default_value_t = 1)]
    pub count: u32,
    /// The display; the primary one by default.
    #[arg(long, value_name = "ID")]
    pub display: Option<String>,
}

/// `gui move`.
#[derive(Debug, Args)]
pub struct MoveArgs {
    /// Pixels from the display's left edge.
    pub x: i32,
    /// Pixels from the display's top edge.
    pub y: i32,
    /// The display; the primary one by default.
    #[arg(long, value_name = "ID")]
    pub display: Option<String>,
}

/// `gui scroll`.
#[derive(Debug, Args)]
pub struct ScrollArgs {
    /// Horizontal notches.
    #[arg(allow_negative_numbers = true)]
    pub dx: i32,
    /// Vertical notches.
    #[arg(allow_negative_numbers = true)]
    pub dy: i32,
    /// Move to `X,Y` first.
    #[arg(long, value_name = "X,Y")]
    pub at: Option<String>,
}

/// `gui type`.
#[derive(Debug, Args)]
pub struct TypeArgs {
    /// The text; `-` reads standard input.
    pub text: String,
}

/// `gui key`.
#[derive(Debug, Args)]
pub struct KeyArgs {
    /// The combinations.
    #[arg(required = true)]
    pub combos: Vec<String>,
}

/// `gui clipboard`.
#[derive(Debug, Args)]
pub struct ClipboardArgs {
    /// Replace the clipboard with this text; `-` reads standard input.
    #[arg(long, value_name = "TEXT")]
    pub set: Option<String>,
}

fn numbers(text: &str, count: usize, what: &str) -> Result<Vec<i64>, String> {
    let parts: Result<Vec<i64>, _> = text.split(',').map(|part| part.trim().parse()).collect();
    match parts {
        Ok(parts) if parts.len() == count => Ok(parts),
        _ => Err(format!("{what} is {count} whole numbers with commas")),
    }
}

fn stdin_text() -> Result<String, String> {
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(|error| error.to_string())?;
    Ok(text)
}

fn button(name: &str) -> Result<Button, String> {
    Ok(match name {
        "left" => Button::Left,
        "right" => Button::Right,
        "middle" => Button::Middle,
        "back" => Button::Back,
        "forward" => Button::Forward,
        _ => return Err("a button is left, right, middle, back or forward".to_owned()),
    })
}

fn id(text: &Option<String>) -> Option<DisplayId> {
    text.as_deref().map(DisplayId::from)
}

/// Runs one `gui` action on `link`.
pub(super) fn run(
    link: &dyn Open,
    args: &RemoteGuiArgs,
    client: &Client,
) -> Result<ExitCode, String> {
    let wait = client.gui;
    let want = |wants: &[Want]| {
        let gui = Gui::open_within(link, &GuiRequest::new(wants.iter().copied()), Some(wait))
            .map_err(|error| explain(&error))?;
        gui.set_image_limit(client.screen.0);
        Ok::<_, String>(gui)
    };
    let fail = |error| explain(&error);
    match &args.action {
        GuiAction::Info => {
            let control = bohio_client::control::Control::open(link).map_err(fail)?;
            let found = control.info(Some(client.reply)).map_err(fail)?;
            if output::json() {
                println!(
                    "{}",
                    serde_json::to_string(&found.gui)
                        .map_err(|_| "the desktop cannot be written")?
                );
            } else {
                print_gui(&found.gui);
            }
        }
        GuiAction::Snapshot(snapshot) => {
            let gui = want(&[Want::View])?;
            let max = snapshot
                .max_size
                .as_deref()
                .map(|text| {
                    let (width, height) = text
                        .split_once(['x', 'X'])
                        .ok_or("--max-size is WIDTHxHEIGHT")?;
                    Ok::<_, String>((
                        width
                            .parse::<u32>()
                            .map_err(|_| "--max-size is WIDTHxHEIGHT")?,
                        height
                            .parse::<u32>()
                            .map_err(|_| "--max-size is WIDTHxHEIGHT")?,
                    ))
                })
                .transpose()?;
            let region = snapshot
                .region
                .as_deref()
                .map(|text| {
                    let n = numbers(text, 4, "--region")?;
                    let part =
                        |at: usize| u32::try_from(n[at]).map_err(|_| "--region is not negative");
                    Ok::<_, String>(Rect::new(part(0)?, part(1)?, part(2)?, part(3)?))
                })
                .transpose()?;
            let format = snapshot
                .format
                .as_deref()
                .map(|name| {
                    ImageFormat::from_name(name).ok_or("--image-format is png, jpeg or webp")
                })
                .transpose()?;
            let options = Snapshot {
                display: id(&snapshot.display),
                region,
                format,
                quality: snapshot.quality,
                max_width: max.map(|size| size.0),
                max_height: max.map(|size| size.1),
                cursor: false,
                settle: snapshot.settle.then(SettleOptions::default),
                ..Snapshot::default()
            };
            let image = gui.snapshot(&options, Some(wait)).map_err(fail)?;
            let to_stdout = snapshot.out.as_os_str() == "-";
            if to_stdout {
                let mut out = std::io::stdout().lock();
                out.write_all(&image.bytes)
                    .map_err(|error| error.to_string())?;
                out.flush().map_err(|error| error.to_string())?;
            } else {
                std::fs::write(&snapshot.out, &image.bytes).map_err(|error| error.to_string())?;
            }
            if output::json() {
                let described = serde_json::json!({
                    "width": image.header.width,
                    "height": image.header.height,
                    "format": image.header.format.to_string(),
                    "display": image.header.display.to_string(),
                    "scale": image.header.scale,
                    "bytes": image.bytes.len(),
                    "path": (!to_stdout).then(|| snapshot.out.display().to_string()),
                });
                // With the image on standard output the description is the final status on standard error.
                if to_stdout {
                    output::status(&serde_json::json!({ "done": described }));
                } else {
                    output::object(&described);
                }
            } else {
                eprintln!(
                    "{}x{} {} display {} scale {}",
                    image.header.width,
                    image.header.height,
                    image.header.format,
                    image.header.display,
                    image.header.scale
                );
            }
        }
        GuiAction::Watch(watch) => {
            let gui = want(&[Want::View])?;
            #[cfg(feature = "gui")]
            gui.set_screen_limits(client.screen.0, client.screen.1);
            watch_screen(&gui, watch, wait)?;
        }
        GuiAction::Click(click) => {
            let gui = want(&[Want::Input])?;
            let event = InputEvent::Button {
                button: button(&click.button)?,
                action: ButtonAction::Click,
                count: Some(click.count),
                display: id(&click.display),
                x: Some(click.x),
                y: Some(click.y),
            };
            gui.input(&[event], Some(wait)).map_err(fail)?;
        }
        GuiAction::Move(go) => {
            let gui = want(&[Want::Input])?;
            let event = InputEvent::Move {
                display: id(&go.display),
                x: go.x,
                y: go.y,
            };
            gui.input(&[event], Some(wait)).map_err(fail)?;
        }
        GuiAction::Scroll(scroll) => {
            let gui = want(&[Want::Input])?;
            let at = scroll
                .at
                .as_deref()
                .map(|text| numbers(text, 2, "--at"))
                .transpose()?;
            let event = InputEvent::Scroll {
                display: None,
                x: at.as_ref().and_then(|n| i32::try_from(n[0]).ok()),
                y: at.as_ref().and_then(|n| i32::try_from(n[1]).ok()),
                dx: scroll.dx,
                dy: scroll.dy,
            };
            gui.input(&[event], Some(wait)).map_err(fail)?;
        }
        GuiAction::Type(text) => {
            let gui = want(&[Want::Input])?;
            let text = if text.text == "-" {
                stdin_text()?
            } else {
                text.text.clone()
            };
            gui.input(&[InputEvent::Text { text }], Some(wait))
                .map_err(fail)?;
        }
        GuiAction::Key(keys) => {
            let gui = want(&[Want::Input])?;
            let mut events = Vec::new();
            for combo in &keys.combos {
                events.extend(InputEvent::combo(combo).map_err(|error| error.to_string())?);
            }
            gui.input(&events, Some(wait)).map_err(fail)?;
        }
        GuiAction::Clipboard(clipboard) => match &clipboard.set {
            Some(text) => {
                let gui = want(&[Want::Clipboard])?;
                let text = if text == "-" {
                    stdin_text()?
                } else {
                    text.clone()
                };
                gui.set_clipboard(&text, Some(wait)).map_err(fail)?;
            }
            None => {
                let gui = want(&[Want::Clipboard])?;
                let text = gui.clipboard(Some(wait)).map_err(fail)?;
                if output::json() {
                    output::object(&serde_json::json!({ "text": text }));
                } else {
                    let mut out = std::io::stdout().lock();
                    out.write_all(text.as_bytes())
                        .map_err(|error| error.to_string())?;
                    out.flush().map_err(|error| error.to_string())?;
                }
            }
        },
    }
    if output::json()
        && matches!(
            &args.action,
            GuiAction::Click(_)
                | GuiAction::Move(_)
                | GuiAction::Scroll(_)
                | GuiAction::Type(_)
                | GuiAction::Key(_)
                | GuiAction::Clipboard(ClipboardArgs { set: Some(_), .. })
        )
    {
        output::object(&serde_json::json!({ "ok": true }));
    }
    Ok(ExitCode::SUCCESS)
}

/// `gui watch`: a watch read for `watch.seconds`, with the numbers that say how the link kept up.
fn watch_screen(gui: &Gui, watch: &WatchArgs, wait: std::time::Duration) -> Result<(), String> {
    use std::time::{Duration, Instant};

    let max = watch
        .max_size
        .as_deref()
        .map(|text| {
            let (width, height) = text
                .split_once(['x', 'X'])
                .ok_or("--max-size is WIDTHxHEIGHT")?;
            Ok::<_, String>((
                width
                    .parse::<u32>()
                    .map_err(|_| "--max-size is WIDTHxHEIGHT")?,
                height
                    .parse::<u32>()
                    .map_err(|_| "--max-size is WIDTHxHEIGHT")?,
            ))
        })
        .transpose()?;
    let region = watch
        .region
        .as_deref()
        .map(|text| {
            let n = numbers(text, 4, "--region")?;
            let part = |at: usize| u32::try_from(n[at]).map_err(|_| "--region is not negative");
            Ok::<_, String>(Rect::new(part(0)?, part(1)?, part(2)?, part(3)?))
        })
        .transpose()?;
    let encoding = watch
        .encoding
        .as_deref()
        .map(|name| match name {
            "auto" => Ok(WatchEncoding::Auto),
            "lossless" => Ok(WatchEncoding::Lossless),
            "jpeg" => Ok(WatchEncoding::Jpeg),
            _ => Err("--encoding is auto, lossless or jpeg"),
        })
        .transpose()?;
    if let Some(dir) = &watch.out {
        std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    gui.watch(&Watch {
        display: id(&watch.display),
        region,
        fps: watch.fps,
        encoding,
        quality: watch.quality,
        max_width: max.map(|size| size.0),
        max_height: max.map(|size| size.1),
        max_rate: watch.max_rate,
        cursor: false,
        refine: None,
        key: false,
        ..Watch::default()
    })
    .map_err(|error| explain(&error))?;
    let started = Instant::now();
    let limit = watch.seconds.map(Duration::from_secs);
    let (mut updates, mut bytes) = (0_u64, 0_u64);
    let (mut second_updates, mut second_bytes) = (0_u64, 0_u64);
    let mut next_line = started + Duration::from_secs(1);
    let (mut level, mut ack_ms) = (0, None);
    loop {
        if limit.is_some_and(|limit| started.elapsed() >= limit) {
            break;
        }
        match gui.next_event(Some(Duration::from_millis(200).min(wait))) {
            Ok(WatchEvent::Update(info)) => {
                updates += 1;
                bytes += info.bytes;
                second_updates += 1;
                second_bytes += info.bytes;
                level = info.header.level;
                ack_ms = info.header.ack_ms;
                if let Some(dir) = &watch.out {
                    write_frame(gui, dir, info.header.frame)?;
                }
            }
            Ok(WatchEvent::Cursor(_)) | Err(bohio_client::Error::Timeout) => {}
            Err(error) => return Err(explain(&error)),
        }
        if watch.stats && Instant::now() >= next_line {
            next_line += Duration::from_secs(1);
            if output::json() {
                output::event(
                    "second",
                    serde_json::json!({
                        "second": started.elapsed().as_secs(),
                        "updates": second_updates,
                        "bytes": second_bytes,
                        "ack_ms": ack_ms,
                        "level": level,
                    }),
                );
                (second_updates, second_bytes) = (0, 0);
                continue;
            }
            println!(
                "{:>4}s {second_updates} updates {second_bytes} bytes ack {} ms level {level}",
                started.elapsed().as_secs(),
                ack_ms.map_or("-".to_owned(), |ms| ms.to_string()),
            );
            (second_updates, second_bytes) = (0, 0);
        }
    }
    if output::json() {
        let seconds = started.elapsed().as_secs_f64();
        output::event(
            "total",
            serde_json::json!({
                "updates": updates,
                "bytes": bytes,
                "seconds": seconds,
                "ack_ms": ack_ms,
                "level": level,
            }),
        );
    } else if watch.stats {
        let seconds = started.elapsed().as_secs_f64().max(f64::EPSILON);
        println!(
            "total {updates} updates in {seconds:.1} s ({:.1} a second), {bytes} bytes ({:.0} an update, {:.0} a second), ack {} ms, level {level}",
            updates as f64 / seconds,
            bytes as f64 / updates.max(1) as f64,
            bytes as f64 / seconds,
            ack_ms.map_or("-".to_owned(), |ms| ms.to_string()),
        );
    }
    gui.unwatch().map_err(|error| explain(&error))?;
    Ok(())
}

/// Writes the composed frame as `frame-N.png` in `dir`.
#[cfg(feature = "gui")]
fn write_frame(gui: &Gui, dir: &std::path::Path, number: u64) -> Result<(), String> {
    let Some(screen) = gui.screen() else {
        return Ok(());
    };
    let image = image::RgbImage::from_raw(screen.width, screen.height, screen.rgb)
        .ok_or("the frame does not match its size")?;
    image
        .save(dir.join(format!("frame-{number}.png")))
        .map_err(|error| error.to_string())
}

#[cfg(not(feature = "gui"))]
fn write_frame(_: &Gui, _: &std::path::Path, _: u64) -> Result<(), String> {
    Err("this build has no image encoders for --out".to_owned())
}

fn print_gui(gui: &bohio_proto::gui::GuiInfo) {
    if !gui.available {
        println!(
            "gui: unavailable ({})",
            gui.reason.as_deref().unwrap_or("no reason given")
        );
        return;
    }
    println!(
        "gui: {} input={} clipboard={} lock_detection={}",
        gui.backend.as_deref().unwrap_or("unknown"),
        gui.input,
        gui.clipboard,
        gui.lock_detection
    );
    for session in &gui.sessions {
        println!(
            "session: {} user={} active={} locked={}",
            session.id,
            session.user.as_deref().unwrap_or("-"),
            session.active,
            session
                .locked
                .map_or("unknown".to_owned(), |locked| locked.to_string())
        );
        for display in &session.displays {
            println!(
                "display: {} {}x{} at {},{} scale {}{}",
                display.id,
                display.width,
                display.height,
                display.x,
                display.y,
                display.scale,
                if display.primary { " primary" } else { "" }
            );
        }
    }
}

/// `bohio gui`: the owner's answer to a request to look at or drive the screen.
#[derive(Debug, Args)]
pub struct GuiArgs {
    /// The configuration.
    #[command(flatten)]
    pub config: ConfigOption,
    /// What to do.
    #[command(subcommand)]
    pub action: ApproveAction,
}

/// The `bohio gui` actions.
#[derive(Debug, Subcommand)]
pub enum ApproveAction {
    /// List the requests waiting for an answer.
    Pending,
    /// Allow request ID: its stream opens, and the peer is not asked again within `[gui] approval_ttl`.
    Allow {
        /// The request, as `pending` prints it.
        id: u64,
    },
    /// Deny request ID.
    Deny {
        /// The request, as `pending` prints it.
        id: u64,
    },
}

/// `bohio gui pending|allow|deny`, through the running node's local control endpoint.
pub(super) fn approve(args: &GuiArgs, environment: &Environment) -> Result<(), String> {
    let (endpoint, settings, server) = control_endpoint(&args.config, environment)?;
    let local = crate::local::LocalLink::connect_to(&endpoint, &settings, server)
        .map_err(|error| unreachable(&error))?;
    match &args.action {
        ApproveAction::Pending => {
            let requests = local.gui_pending().map_err(|error| explain(&error))?;
            if output::json() {
                output::object(&serde_json::json!({
                    "pending": requests.iter().map(|request| serde_json::json!({
                        "id": request.id,
                        "peer": request.peer.to_string(),
                        "session": request.session,
                        "want": request.want.iter().map(|want| want.name()).collect::<Vec<_>>(),
                    })).collect::<Vec<_>>(),
                }));
                return Ok(());
            }
            if requests.is_empty() {
                println!("nothing is waiting");
            }
            for request in requests {
                let want: Vec<&str> = request.want.iter().map(|want| want.name()).collect();
                println!(
                    "{} peer {} session {} wants {}",
                    request.id,
                    request.peer,
                    request.session,
                    want.join(",")
                );
            }
        }
        ApproveAction::Allow { id } | ApproveAction::Deny { id } => {
            let allow = matches!(args.action, ApproveAction::Allow { .. });
            let found = local
                .gui_decide(*id, allow)
                .map_err(|error| explain(&error))?;
            if !found {
                return Err(format!(
                    "request {id} is not waiting: it timed out or was answered"
                ));
            }
            if output::json() {
                let mut answer = serde_json::Map::new();
                answer.insert(
                    if allow { "allowed" } else { "denied" }.to_owned(),
                    serde_json::json!(id),
                );
                output::object(&serde_json::Value::Object(answer));
            } else {
                println!("{} {id}", if allow { "allowed" } else { "denied" });
            }
        }
    }
    Ok(())
}
