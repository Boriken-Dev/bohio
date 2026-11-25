//! A node that serves a synthetic desktop over TCP, for measuring a watch with the built `bohio` command from another
//! machine: `gui_host --listen tcp://127.0.0.1:0 --peer sha256:... --scene motion --size 1920x1080 [--gui 'adapt = false']
//! [--allow view,input,clipboard] [--seconds N]`. It prints `id <peer id>` and `listening <endpoint>`, grants the peers it was
//! given `gui.view` (or what `--allow` names), and runs until it is killed or `--seconds` have passed. The `--gui` text is
//! `[gui]` settings in TOML.
//!
//! `--scene painted` serves the tests' painted desktop instead (two displays, 320 by 200 and 160 by 100, input that records
//! and paints), and prints what it takes, one line each as it happens: `event <what>` for an input event
//! (`click:leftx1@0:160,100`, `text:hi`, `key:s:click`) and `clipboard <text>` when the clipboard changes.

#![forbid(unsafe_code)]

#[path = "support/painted.rs"]
mod painted;
#[path = "support/synthetic.rs"]
mod synthetic;

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::Duration;

use bohio::config::{GuiConfig, ListenConfig, NodeConfig};
use bohio::listen::Listening;
use bohio::node::{MemoryGrants, MemoryKeys, Node};
use bohio::policy::{Grant, Permission};
use cabuya::auth::{Identity, Method};
use coqui_peer::PeerId;
use painted::Painted;
use synthetic::{Scene, Synthetic};

fn main() -> Result<(), String> {
    let mut listen = "tcp://127.0.0.1:0".to_owned();
    let mut peers = Vec::new();
    let mut scene = Some(Scene::Motion);
    let mut allow = vec![Permission::GuiView];
    let (mut width, mut height) = (1920_u32, 1080_u32);
    let mut gui = GuiConfig::default();
    let mut seconds: Option<u64> = None;
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--listen" => listen = value()?,
            "--peer" => peers.push(value()?.parse::<PeerId>().map_err(|_| "a peer id")?),
            "--scene" => {
                let name = value()?;
                scene = match name.as_str() {
                    "painted" => None,
                    other => Some(Scene::named(other).ok_or("--scene is text, motion or painted")?),
                };
            }
            "--allow" => {
                allow = value()?
                    .split(',')
                    .map(|name| match name.trim() {
                        "view" => Ok(Permission::GuiView),
                        "input" => Ok(Permission::GuiInput),
                        "clipboard" => Ok(Permission::GuiClipboard),
                        other => Err(format!(
                            "--allow takes view, input and clipboard, not {other}"
                        )),
                    })
                    .collect::<Result<_, _>>()?;
            }
            "--size" => {
                let text = value()?;
                let (w, h) = text.split_once('x').ok_or("--size is WIDTHxHEIGHT")?;
                width = w.parse().map_err(|_| "--size is WIDTHxHEIGHT")?;
                height = h.parse().map_err(|_| "--size is WIDTHxHEIGHT")?;
            }
            "--gui" => gui = toml::from_str(&value()?).map_err(|error| error.to_string())?,
            "--seconds" => seconds = Some(value()?.parse().map_err(|_| "--seconds is a number")?),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    gui.enabled = true;
    let config = NodeConfig {
        gui,
        ..NodeConfig::default()
    };
    let identity = Identity::generate(Method::DEFAULT).map_err(|error| error.to_string())?;
    let id = identity.peer_id();
    let keys = Arc::new(MemoryKeys::new());
    let grants = Arc::new(MemoryGrants::new());
    for peer in &peers {
        keys.accept(*peer);
        grants.set(
            *peer,
            Grant::allowing(std::iter::once(Permission::Info).chain(allow.iter().copied())),
        );
    }
    let node =
        Node::new(config.clone(), identity, keys, grants).map_err(|error| error.to_string())?;
    let stop = Arc::new(AtomicBool::new(false));
    let node = match scene {
        Some(scene) => node.with_desktop(Arc::new(Synthetic::new(width, height, scene))),
        None => {
            let desktop = Arc::new(Painted::new());
            report(Arc::clone(&desktop), Arc::clone(&stop));
            node.with_desktop(desktop)
        }
    };
    let entry = ListenConfig {
        endpoint: listen,
        ..ListenConfig::default()
    };
    let (listening, bound) =
        Listening::bind(&entry, &config.link.settings()).map_err(|error| error.to_string())?;
    println!("id {id}");
    println!("listening {bound}");
    std::io::stdout()
        .flush()
        .map_err(|error| error.to_string())?;
    if let Some(seconds) = seconds {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(seconds));
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        });
    }
    node.serve_listening(&listening, &stop)
        .map_err(|error| error.to_string())
}

/// Prints what the painted desktop takes as it takes it, until `stop` is set.
fn report(desktop: Arc<Painted>, stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        let (mut seen, mut clipboard) = (0, String::new());
        while !stop.load(std::sync::atomic::Ordering::SeqCst) {
            let log = desktop.log();
            let now = desktop.clipboard();
            let mut out = std::io::stdout().lock();
            for line in log.iter().skip(seen) {
                let _ = writeln!(out, "event {line}");
            }
            seen = log.len();
            if now != clipboard {
                let _ = writeln!(out, "clipboard {now}");
                clipboard = now;
            }
            let _ = out.flush();
            drop(out);
            thread::sleep(Duration::from_millis(10));
        }
    });
}
