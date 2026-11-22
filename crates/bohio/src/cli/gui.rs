//! `bohio remote ... gui` (look at and drive a host's desktop) and `bohio gui` (the owner answering a request to look).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use bohio_client::Open;
use bohio_client::gui::{Gui, SnapshotOptions};
use bohio_proto::gui::{
    Button, ButtonAction, DisplayId, GuiRequest, ImageFormat, InputEvent, Rect, SettleOptions, Want,
};
use clap::{Args, Subcommand};

use super::remote::{control_endpoint, explain, unreachable};
use super::{ConfigOption, Format};
use crate::config::Environment;

const WAIT: Duration = Duration::from_secs(120);

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
    Info(GuiInfoArgs),
    /// Capture a display, or a region of one, to a file or standard output.
    Snapshot(SnapshotArgs),
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

/// `gui info`.
#[derive(Debug, Args)]
pub struct GuiInfoArgs {
    /// The output format.
    #[arg(long, value_enum, default_value = "text")]
    pub format: Format,
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
    #[arg(long, value_name = "FORMAT")]
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
pub(super) fn run(link: &dyn Open, args: &RemoteGuiArgs) -> Result<ExitCode, String> {
    let want = |wants: &[Want]| {
        Gui::open(link, &GuiRequest::new(wants.iter().copied())).map_err(|error| explain(&error))
    };
    let fail = |error| explain(&error);
    match &args.action {
        GuiAction::Info(info) => {
            let control = bohio_client::control::Control::open(link).map_err(fail)?;
            let found = control.info(Some(Duration::from_secs(30))).map_err(fail)?;
            match info.format {
                Format::Json => println!(
                    "{}",
                    serde_json::to_string(&found.gui)
                        .map_err(|_| "the desktop cannot be written")?
                ),
                Format::Text => print_gui(&found.gui),
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
                .map(|name| ImageFormat::from_name(name).ok_or("--format is png, jpeg or webp"))
                .transpose()?;
            let options = SnapshotOptions {
                display: id(&snapshot.display),
                region,
                format,
                quality: snapshot.quality,
                max_width: max.map(|size| size.0),
                max_height: max.map(|size| size.1),
                cursor: false,
                settle: snapshot.settle.then(SettleOptions::default),
            };
            let image = gui.snapshot(&options, Some(WAIT)).map_err(fail)?;
            if snapshot.out.as_os_str() == "-" {
                let mut out = std::io::stdout().lock();
                out.write_all(&image.bytes)
                    .map_err(|error| error.to_string())?;
                out.flush().map_err(|error| error.to_string())?;
            } else {
                std::fs::write(&snapshot.out, &image.bytes).map_err(|error| error.to_string())?;
            }
            eprintln!(
                "{}x{} {} display {} scale {}",
                image.header.width,
                image.header.height,
                image.header.format,
                image.header.display,
                image.header.scale
            );
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
            gui.input(&[event], Some(WAIT)).map_err(fail)?;
        }
        GuiAction::Move(go) => {
            let gui = want(&[Want::Input])?;
            let event = InputEvent::Move {
                display: id(&go.display),
                x: go.x,
                y: go.y,
            };
            gui.input(&[event], Some(WAIT)).map_err(fail)?;
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
            gui.input(&[event], Some(WAIT)).map_err(fail)?;
        }
        GuiAction::Type(text) => {
            let gui = want(&[Want::Input])?;
            let text = if text.text == "-" {
                stdin_text()?
            } else {
                text.text.clone()
            };
            gui.input(&[InputEvent::Text { text }], Some(WAIT))
                .map_err(fail)?;
        }
        GuiAction::Key(keys) => {
            let gui = want(&[Want::Input])?;
            let mut events = Vec::new();
            for combo in &keys.combos {
                events.extend(InputEvent::combo(combo).map_err(|error| error.to_string())?);
            }
            gui.input(&events, Some(WAIT)).map_err(fail)?;
        }
        GuiAction::Clipboard(clipboard) => match &clipboard.set {
            Some(text) => {
                let gui = want(&[Want::Clipboard])?;
                let text = if text == "-" {
                    stdin_text()?
                } else {
                    text.clone()
                };
                gui.set_clipboard(&text, Some(WAIT)).map_err(fail)?;
            }
            None => {
                let gui = want(&[Want::Clipboard])?;
                let text = gui.clipboard(Some(WAIT)).map_err(fail)?;
                let mut out = std::io::stdout().lock();
                out.write_all(text.as_bytes())
                    .map_err(|error| error.to_string())?;
                out.flush().map_err(|error| error.to_string())?;
            }
        },
    }
    Ok(ExitCode::SUCCESS)
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
            println!("{} {id}", if allow { "allowed" } else { "denied" });
        }
    }
    Ok(())
}
