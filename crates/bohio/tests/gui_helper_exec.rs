//! A GUI helper that runs on another machine: the node starts `bohio gui-helper --stdio` through an ssh source (an in-process
//! SSH server whose `sh` runs the command), serves its desktop as a session, and a peer sees and clicks it. The helper is the
//! painted desktop (`examples/gui_helper_painted.rs`): no screen is touched.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::access::Sources;
use bohio::config::{AccessConfig, GuiConfig, NodeConfig, SourceTables};
use bohio::gui::helper_exec::ExecHelper;
use bohio::node::{MemoryGrants, MemoryKeys, Node};
use bohio::policy::{Grant, Permission};
use bohio_client::gui::{Gui, SnapshotOptions};
use bohio_client::link::{Settings, stream};
use bohio_proto::gui::{Button, ButtonAction, GuiRequest, InputEvent, Want};
use cabuya::auth::{Identity, Method};
use coqui_transport::{Channel as _, memory};

#[path = "../../../providers/ssh/tests/support/mod.rs"]
mod support;
use support::{FakeSsh, shell_available};

const WAIT: Duration = Duration::from_secs(60);

fn helper_program() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let name = format!("gui_helper_painted{}", std::env::consts::EXE_SUFFIX);
    let path = exe.parent()?.parent()?.join("examples").join(name);
    path.is_file().then(|| path.to_string_lossy().into_owned())
}

/// Runs the scenario with `command` as the helper's command line; `width` is the first display's width when it is known.
fn scenario(command: Vec<String>, width: Option<u32>) {
    let server = FakeSsh::start();
    let text = format!(
        "[source.remote]\nprovider = \"ssh\"\nhost = \"127.0.0.1\"\nport = {}\nuser = \"{}\"\nidentity = {:?}\nhost_key = \"{}\"\nagent = false\n",
        server.port,
        support::USER,
        server.identity().to_string_lossy(),
        server.pin()
    );
    #[derive(serde::Deserialize)]
    struct Whole {
        source: SourceTables,
    }
    let tables = toml::from_str::<Whole>(&text)
        .expect("source tables")
        .source;
    let sources = Sources::unconfigured();
    sources
        .reconfigure(
            &tables,
            &AccessConfig {
                exec: Some(vec!["remote".to_owned()]),
                desktop: Some(Vec::new()),
                ..AccessConfig::default()
            },
        )
        .expect("the sources build");

    let make = |sources: Option<Sources>, gui: GuiConfig| {
        let identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let id = identity.peer_id();
        let keys = Arc::new(MemoryKeys::new());
        let grants = Arc::new(MemoryGrants::new());
        let mut node = Node::new(
            NodeConfig {
                gui,
                ..NodeConfig::default()
            },
            identity,
            keys.clone(),
            grants.clone(),
        )
        .expect("the node is built")
        .with_path(std::env::var_os("PATH").unwrap_or_default());
        if let Some(sources) = sources {
            node = node.with_sources(sources);
        }
        (Arc::new(node), id, keys, grants)
    };
    let gui = GuiConfig {
        enabled: true,
        ..GuiConfig::default()
    };
    let (host, host_id, host_keys, host_grants) = make(Some(sources), gui);
    let (client, client_id, client_keys, _) = make(None, GuiConfig::default());
    host_keys.accept(client_id);
    host_grants.set(
        client_id,
        Grant::allowing([Permission::Info, Permission::GuiView, Permission::GuiInput]),
    );
    client_keys.accept(host_id);
    let settings = Settings::default();
    let (one, two) = memory::pipe(1024 * 1024);
    let dial_end = Arc::new(stream(one, &settings).expect("a channel"));
    let answer_end = Arc::new(stream(two, &settings).expect("a channel"));
    let stop = Arc::new(AtomicBool::new(false));
    let threads: Vec<JoinHandle<()>> = vec![
        thread::spawn({
            let (node, channel) = (client.clone(), dial_end.clone());
            move || {
                let _ = node.connect_channel(&*channel, &host_id, None);
                channel.close();
            }
        }),
        thread::spawn({
            let (node, channel) = (host.clone(), answer_end.clone());
            move || {
                node.serve_channel(&*channel, None);
                channel.close();
            }
        }),
        thread::spawn({
            let (node, stop) = (host.clone(), stop.clone());
            move || {
                node.serve_exec_helpers(
                    &[ExecHelper {
                        source: "remote".to_owned(),
                        command: command.clone(),
                    }],
                    Duration::from_millis(200),
                    &stop,
                );
            }
        }),
    ];
    let until = Instant::now() + WAIT;
    let link = loop {
        if let Some(link) = client.peer(&host_id) {
            break link;
        }
        assert!(Instant::now() < until, "no link came up");
        thread::sleep(Duration::from_millis(20));
    };

    // The helper's session is listed once it has connected.
    let gui = loop {
        match Gui::open(&link, &GuiRequest::new([Want::View, Want::Input])) {
            Ok(gui) => break gui,
            Err(error) => {
                assert!(
                    Instant::now() < until,
                    "the helper never came up: {error:?}"
                );
                thread::sleep(Duration::from_millis(200));
            }
        }
    };
    assert!(
        gui.session().ends_with(":remote"),
        "the session is the helper's: {}",
        gui.session()
    );
    let image = gui
        .snapshot(&SnapshotOptions::default(), Some(WAIT))
        .expect("a snapshot through the helper");
    match width {
        Some(width) => assert_eq!(image.header.width, width),
        None => assert!(image.header.width > 0),
    }
    let click = InputEvent::Button {
        button: Button::Left,
        action: ButtonAction::Click,
        count: None,
        display: None,
        x: Some(100),
        y: Some(50),
    };
    // Only the painted desktop is clicked: a click on a person's real screen is not a test's to make.
    if width.is_some() {
        assert_eq!(
            gui.input(&[click], Some(WAIT))
                .expect("input through the helper"),
            1
        );
    }
    gui.close();

    stop.store(true, Ordering::SeqCst);
    dial_end.close();
    answer_end.close();
    for handle in threads {
        let _ = handle.join();
    }
}

#[test]
fn a_peer_sees_and_clicks_the_desktop_of_a_helper_the_node_runs_through_an_ssh_source() {
    if !shell_available() {
        eprintln!("SKIPPED: no `sh` on PATH for the in-process SSH server to run commands");
        return;
    }
    let Some(program) = helper_program() else {
        eprintln!(
            "SKIPPED: build the helper first (cargo build -p bohio --example gui_helper_painted)"
        );
        return;
    };
    scenario(vec![program], Some(320));
}

/// The real helper, `bohio gui-helper --stdio`, on the machine's own X display.
#[cfg(target_os = "linux")]
#[test]
fn the_real_helper_serves_the_machines_x_display_through_an_ssh_source() {
    if !shell_available() {
        eprintln!("SKIPPED: no `sh` on PATH for the in-process SSH server to run commands");
        return;
    }
    let Some(display) = std::fs::read_dir("/tmp/.X11-unix")
        .ok()
        .and_then(|mut found| found.find_map(Result::ok))
        .and_then(|entry| {
            entry
                .file_name()
                .to_str()?
                .strip_prefix('X')
                .map(|number| format!(":{number}"))
        })
    else {
        eprintln!("SKIPPED: this machine has no X server");
        return;
    };
    scenario(
        vec![
            "env".to_owned(),
            "-u".to_owned(),
            "WAYLAND_DISPLAY".to_owned(),
            format!("DISPLAY={display}"),
            env!("CARGO_BIN_EXE_bohio").to_owned(),
            "gui-helper".to_owned(),
            "--stdio".to_owned(),
            "--session".to_owned(),
            "remote".to_owned(),
        ],
        None,
    );
}
