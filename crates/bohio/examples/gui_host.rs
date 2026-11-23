//! A node that serves a synthetic desktop over TCP, for measuring a watch with the built `bohio` command from another
//! machine: `gui_host --listen tcp://127.0.0.1:0 --peer sha256:... --scene motion --size 1920x1080 [--gui 'adapt = false']
//! [--seconds N]`. It prints `id <peer id>` and `listening <endpoint>`, grants the peers it was given `gui.view`, and runs
//! until it is killed or `--seconds` have passed. The `--gui` text is `[gui]` settings in TOML.

#![forbid(unsafe_code)]

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
use coqui_link::auth::{Identity, Method};
use coqui_peer::PeerId;
use synthetic::{Scene, Synthetic};

fn main() -> Result<(), String> {
    let mut listen = "tcp://127.0.0.1:0".to_owned();
    let mut peers = Vec::new();
    let mut scene = Scene::Motion;
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
                scene = Scene::named(&value()?).ok_or("--scene is text or motion")?;
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
            Grant::allowing([Permission::Info, Permission::GuiView]),
        );
    }
    let node = Node::new(config.clone(), identity, keys, grants)
        .map_err(|error| error.to_string())?
        .with_desktop(Arc::new(Synthetic::new(width, height, scene)));
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
    let stop = Arc::new(AtomicBool::new(false));
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
