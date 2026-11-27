//! A node's console: whatever its terminal source serves, with no program started. Over an in-memory source on every
//! platform, and over a serial line (a pair of pseudo-terminals from `socat`) where there is one.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::access::{Capability, Sources};
use bohio::config::{AccessConfig, NodeConfig, SourceTables, Span};
use bohio::node::{MemoryGrants, MemoryKeys, Node, PeerLink};
use bohio::policy::{Grant, Permission};
use bohio_api::testing::{self, TerminalLink};
use bohio_client::Error;
use bohio_client::control::Control;
use bohio_client::link::{Settings, stream};
use bohio_client::shell::{Event, Shell};
use bohio_proto::shell::{Request, ShellKind};
use bohio_proto::signal::Signal;
use bohio_proto::{ErrorBody, ErrorCode};
use cabuya::auth::{Identity, Method};
use coqui_transport::{Channel, StreamChannel, memory};

const WAIT: Duration = Duration::from_secs(30);
const RECHECK: Duration = Duration::from_millis(400);

struct Wire {
    channels: [Arc<StreamChannel>; 2],
    threads: Vec<JoinHandle<()>>,
}

impl Drop for Wire {
    fn drop(&mut self) {
        for channel in &self.channels {
            channel.close();
        }
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

struct World {
    host: Arc<Node>,
    link: PeerLink,
    /// The host's grants, to narrow while a stream runs.
    grants: Arc<MemoryGrants>,
    client_id: coqui_peer::PeerId,
    _wire: Wire,
}

fn tables(text: &str) -> SourceTables {
    #[derive(serde::Deserialize)]
    struct Whole {
        source: SourceTables,
    }
    toml::from_str::<Whole>(text).expect("source tables").source
}

/// A host whose only terminal source is the in-memory console `link` (or the serial line named by `serial`), and a
/// client granted `grant`, linked over memory pipes.
fn world(source_toml: &str, grant: Grant) -> World {
    world_with(source_toml, &["console"], grant)
}

fn world_with(source_toml: &str, order: &[&str], grant: Grant) -> World {
    let sources = Sources::unconfigured();
    sources
        .register_library("bohio-api-testing", testing::library)
        .expect("the fake providers register");
    sources
        .reconfigure(
            &tables(source_toml),
            &AccessConfig {
                terminal: Some(order.iter().map(|name| (*name).to_owned()).collect()),
                desktop: Some(Vec::new()),
                fs: Some(Vec::new()),
                exec: Some(Vec::new()),
                ..AccessConfig::default()
            },
        )
        .expect("the sources build");
    let make = |sources: Option<Sources>, recheck: Duration| {
        let identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let id = identity.peer_id();
        let keys = Arc::new(MemoryKeys::new());
        let grants = Arc::new(MemoryGrants::new());
        let mut node = Node::new(
            NodeConfig {
                link: bohio::config::LinkConfig {
                    recheck: Span(recheck),
                    ..bohio::config::LinkConfig::default()
                },
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
    let (host, host_id, host_keys, host_grants) = make(Some(sources), RECHECK);
    let (client, client_id, client_keys, _) = make(None, RECHECK);
    host_keys.accept(client_id);
    host_grants.set(client_id, grant);
    client_keys.accept(host_id);
    let settings = Settings::default();
    let (one, two) = memory::pipe(1024 * 1024);
    let dial_end = Arc::new(stream(one, &settings).expect("a channel"));
    let answer_end = Arc::new(stream(two, &settings).expect("a channel"));
    let threads = vec![
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
    ];
    let until = Instant::now() + WAIT;
    let link = loop {
        if let Some(link) = client.peer(&host_id) {
            break link;
        }
        assert!(Instant::now() < until, "no link came up");
        thread::sleep(Duration::from_millis(20));
    };
    World {
        host,
        link,
        grants: host_grants,
        client_id,
        _wire: Wire {
            channels: [dial_end, answer_end],
            threads,
        },
    }
}

fn granted() -> Grant {
    Grant::allowing([Permission::Info, Permission::Shell, Permission::Session])
}

fn fake(link: &str) -> String {
    format!("[source.console]\nprovider = \"fake_terminal\"\nlink = \"{link}\"\n")
}

fn remote(error: &Error) -> &ErrorBody {
    match error {
        Error::Remote(body) => body,
        other => panic!("expected the host's error, got {other:?}"),
    }
}

fn console(world: &World) -> Shell {
    Shell::start(&world.link, &Request::console(None), Some(WAIT)).expect("the console opens")
}

fn heard(shell: &Shell, needle: &str) -> Vec<u8> {
    shell
        .read_until(needle.as_bytes(), Some(WAIT))
        .unwrap_or_else(|error| panic!("`{needle}` never came: {error:?}"))
}

#[test]
fn a_peer_attaches_types_detaches_and_reattaches_for_the_scrollback() {
    let link: Arc<TerminalLink> = testing::terminal_link("console-attach");
    let world = world(&fake("console-attach"), granted());
    let shell = console(&world);
    link.device_writes(b"login: ");
    heard(&shell, "login: ");
    shell.write(b"root\r").expect("typing");
    assert!(
        link.typed_within(b"root\r", WAIT).ends_with(b"root\r"),
        "the far end read what was typed"
    );
    // Detaching leaves the console open.
    drop(shell);
    link.device_writes(b"Password: ");
    thread::sleep(Duration::from_millis(300));
    // The console is a session the node owns, listed beside detached ones.
    let sessions = Control::open(&world.link)
        .expect("control")
        .sessions(Some(WAIT))
        .expect("sessions");
    assert!(
        sessions
            .iter()
            .any(|session| session.id == "console:console"),
        "{sessions:?}"
    );
    let (again, attached) = Shell::attach(&world.link, "console:console", Some(0))
        .expect("the console is picked up again");
    assert_eq!(attached.lost, 0);
    let output = heard(&again, "Password: ");
    assert!(
        String::from_utf8_lossy(&output).contains("login: "),
        "the scrollback holds what came before: {output:?}"
    );
    assert_eq!(
        link.opened(),
        1,
        "the source was opened once for the whole time"
    );
}

#[test]
fn the_agent_session_tools_reach_a_console_with_no_new_tool() {
    use bohio_client::tools::{Cancel, Tools};
    let link: Arc<TerminalLink> = testing::terminal_link("console-tools");
    let world = world(&fake("console-tools"), granted());
    let shell = console(&world);
    link.device_writes(
        b"boot ok
",
    );
    heard(&shell, "boot ok");
    drop(shell);
    let tools = Tools::new(Arc::new(world.link.clone())).expect("the tools are made");
    let call = |name: &str, arguments: serde_json::Value| {
        tools
            .call(name, arguments, &Cancel::new())
            .unwrap_or_else(|error| panic!("{name} failed: {error}"))
            .value
    };
    let listed = call("session_list", serde_json::json!({}));
    assert!(
        listed["sessions"]
            .as_array()
            .expect("a list")
            .iter()
            .any(|session| session["id"] == "console:console"),
        "{listed}"
    );
    let read = call(
        "session_read",
        serde_json::json!({"session": "console:console", "from": 0}),
    );
    assert!(read.to_string().contains("boot ok"), "{read}");
}

#[test]
fn info_access_lists_the_console_with_its_limits_and_asks_the_device_every_time() {
    let link = testing::terminal_link("console-info");
    let world = world(&fake("console-info"), granted());
    let info = || {
        Control::open(&world.link)
            .expect("control")
            .info(Some(WAIT))
            .expect("info")
    };
    let first = info();
    assert!(first.access.fs.is_empty());
    assert!(first.access.exec.is_empty());
    assert_eq!(first.access.terminal.len(), 1);
    let source = &first.access.terminal[0];
    assert_eq!(
        (source.name.as_str(), source.provider.as_str()),
        ("console", "fake_terminal")
    );
    assert!(source.default && source.available);
    for word in ["resize", "signals", "exit_status", "programs"] {
        assert!(
            source.lacks.iter().any(|lacks| lacks == word),
            "{word}: {source:?}"
        );
    }
    assert!(first.access.desktop.is_empty() && first.access.power.is_empty());

    // The device is unplugged: the next `info` says so, with the reason, and the one after the plug says it is back.
    link.set_available(false);
    let gone = info();
    assert!(!gone.access.terminal[0].available);
    assert!(gone.access.terminal[0].reason.is_some());
    link.set_available(true);
    assert!(info().access.terminal[0].available);
}

#[test]
fn a_peer_without_sources_reaches_only_the_default_and_a_later_source_grants_it_nothing() {
    let both = format!(
        "{}[source.spare]
provider = \"fake_terminal\"
link = \"console-spare\"
",
        fake("console-default")
    );
    let spare = testing::terminal_link("console-spare");
    let world = world_with(&both, &["console", "spare"], granted());
    let info = Control::open(&world.link)
        .expect("control")
        .info(Some(WAIT))
        .expect("info");
    let seen: Vec<&str> = info
        .access
        .terminal
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(seen, ["console"], "the other source is not even named");
    let denied = Shell::start(&world.link, &Request::console(Some("spare")), Some(WAIT))
        .map(drop)
        .expect_err("a source the grant does not reach");
    // The link refuses the open itself (`Permission`), before the engine is asked.
    assert!(
        matches!(&denied, Error::Transport(_))
            || matches!(&denied, Error::Remote(body) if body.code == ErrorCode::Denied),
        "{denied:?}"
    );
    assert_eq!(spare.opened(), 0, "nothing reached the source");
    assert!(Shell::start(&world.link, &Request::console(Some("console")), Some(WAIT)).is_ok());
    let sessions = Control::open(&world.link)
        .expect("control")
        .sessions(Some(WAIT))
        .expect("sessions");
    assert!(sessions.iter().all(|session| session.id != "console:spare"));

    // `sources = [..]` names more.
    let wider = world_with(
        &both,
        &["console", "spare"],
        granted().with_sources(["spare"]),
    );
    let info = Control::open(&wider.link)
        .expect("control")
        .info(Some(WAIT))
        .expect("info");
    assert_eq!(info.access.terminal.len(), 2);
    let shell =
        Shell::start(&wider.link, &Request::console(Some("spare")), Some(WAIT)).expect("allowed");
    spare.device_writes(b"spare up");
    heard(&shell, "spare up");

    // A source added later grants nobody anything: the first world's peer still reaches the default alone.
    world
        .host
        .sources()
        .add_terminal("late", Arc::new(testing::FakeTerminal::default()));
    let info = Control::open(&world.link)
        .expect("control")
        .info(Some(WAIT))
        .expect("info");
    assert_eq!(info.access.terminal.len(), 1);
}

#[test]
fn a_peer_cannot_attach_to_a_console_it_does_not_reach_and_a_revoke_ends_its_attach() {
    let both = format!(
        "{}[source.spare]\nprovider = \"fake_terminal\"\nlink = \"console-revoke-spare\"\n",
        fake("console-revoke")
    );
    let spare = testing::terminal_link("console-revoke-spare");
    let wider = world_with(
        &both,
        &["console", "spare"],
        granted().with_sources(["spare"]),
    );
    let shell =
        Shell::start(&wider.link, &Request::console(Some("spare")), Some(WAIT)).expect("allowed");
    spare.device_writes(b"up ");
    heard(&shell, "up ");

    // Another peer, with the defaults alone, cannot pick that console up by its id.
    let narrow = world_with(&both, &["console", "spare"], granted());
    let refused = Shell::attach(&narrow.link, "console:spare", Some(0))
        .map(drop)
        .expect_err("not reached");
    assert!(
        matches!(&refused, Error::Transport(_))
            || matches!(&refused, Error::Remote(body) if body.code == ErrorCode::NotFound),
        "{refused:?}"
    );

    // The grant loses `spare`: the attach ends within the recheck.
    wider.grants.set(wider.client_id, granted());
    let until = Instant::now() + Duration::from_secs(10);
    while let Err(Error::Timeout) | Ok(Some(Event::Data(_))) =
        shell.next(Some(Duration::from_millis(200)))
    {
        assert!(Instant::now() < until, "the attach outlived its grant");
    }
}

#[test]
fn a_second_attach_supersedes_the_first() {
    let link = testing::terminal_link("console-supersede");
    let world = world(&fake("console-supersede"), granted());
    let first = console(&world);
    link.device_writes(b"one ");
    heard(&first, "one ");
    let second = console(&world);
    link.device_writes(b"two ");
    heard(&second, "two ");
    let until = Instant::now() + WAIT;
    loop {
        match first.next(Some(Duration::from_millis(200))) {
            Err(Error::Timeout) => {
                assert!(Instant::now() < until, "the first attach was never ended")
            }
            Ok(Some(Event::Data(_))) => {}
            Ok(Some(_) | None) | Err(_) => break,
        }
    }
}

/// Two first attaches that arrive together make one console: the engine the loser started is ended, not left outside
/// the session map where nothing would end it.
#[test]
fn two_first_attaches_make_one_console_and_leave_no_engine_behind() {
    for round in 0..10 {
        let name = format!("console-race-{round}");
        let link = testing::terminal_link(&name);
        let world = world(&fake(&name), granted());
        let start = std::sync::Barrier::new(2);
        let attached: Vec<Shell> = thread::scope(|scope| {
            let attach = || {
                scope.spawn(|| {
                    start.wait();
                    console(&world)
                })
            };
            let (a, b) = (attach(), attach());
            vec![a.join().unwrap(), b.join().unwrap()]
        });
        link.device_writes(b"one console");
        for shell in &attached {
            // The one that came second supersedes the first; whichever is current hears the device.
            let _ = shell.next(Some(Duration::from_millis(200)));
        }
        drop(attached);
        drop(world);
        // The host's stop ends every console it holds, and every engine started for one.
        let until = Instant::now() + WAIT;
        while link.killed() < link.opened() {
            assert!(
                Instant::now() < until,
                "round {round}: {} terminals opened and {} ended",
                link.opened(),
                link.killed()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

/// Waits for the host to end `shell`'s attach: the stream closes with no exit.
fn assert_detached(shell: &Shell) {
    let until = Instant::now() + WAIT;
    loop {
        match shell.next(Some(Duration::from_millis(200))) {
            Err(Error::Timeout) | Ok(Some(Event::Data(_))) => {
                assert!(Instant::now() < until, "the kill never ended the attach");
            }
            Ok(Some(Event::Exit(exit))) => panic!("a console has no exit: {exit:?}"),
            Ok(Some(_) | None) | Err(_) => return,
        }
    }
}

#[test]
fn a_program_is_unsupported_naming_the_source_and_a_kill_only_detaches() {
    let link = testing::terminal_link("console-kind");
    let world = world(&fake("console-kind"), granted());
    let refused = Shell::start(&world.link, &Request::kind(ShellKind::Sh), Some(WAIT))
        .map(drop)
        .expect_err("a console starts no program");
    let body = remote(&refused);
    assert_eq!(body.code, ErrorCode::Unsupported);
    assert!(body.message.contains("console"), "{}", body.message);
    assert!(
        body.message.contains("starts no program"),
        "{}",
        body.message
    );

    let shell = console(&world);
    shell.kill().expect("the kill is sent");
    assert_detached(&shell);
    thread::sleep(Duration::from_millis(500));
    assert_eq!(link.killed(), 0, "a client kill never ends the source");
    drop(shell);

    // A kill signal is answered as not delivered, and the attach ends too.
    let shell = console(&world);
    shell.signal(Signal::Kill).expect("the signal is sent");
    let mut answered = None;
    let until = Instant::now() + WAIT;
    while Instant::now() < until {
        match shell.next(Some(Duration::from_millis(200))) {
            Ok(Some(Event::Signalled(answer))) => answered = Some(answer),
            Ok(Some(Event::Data(_))) | Err(Error::Timeout) => {}
            Ok(Some(Event::Exit(_)) | None) | Err(_) => break,
            Ok(Some(_)) => {}
        }
    }
    let answer = answered.expect("a kill signal is answered");
    assert!(!answer.delivered, "{answer:?}");
    drop(shell);
    let again = console(&world);
    link.device_writes(b"still here");
    heard(&again, "still here");
    assert_eq!(link.opened(), 1);
}

#[test]
fn a_node_with_no_default_terminal_source_has_no_console() {
    let world = world(&fake("console-none"), granted());
    world
        .host
        .sources()
        .reconfigure(
            &SourceTables::new(),
            &AccessConfig {
                terminal: Some(Vec::new()),
                ..AccessConfig::default()
            },
        )
        .expect("an empty list");
    assert!(world.host.sources().names(Capability::Terminal).is_empty());
    let refused = Shell::start(&world.link, &Request::console(None), Some(WAIT))
        .map(drop)
        .expect_err("no source");
    assert_eq!(remote(&refused).code, ErrorCode::Unsupported);
}

#[test]
fn native_passes_the_terminal_contract() {
    use bohio_api::contract::{self, ContractSettings};
    use bohio_api::model::{ProgramSpec, TerminalOpen};
    let sources = Sources::default();
    let native = sources
        .terminal(Some("native"))
        .expect("native is a source");
    let open = TerminalOpen {
        program: Some(ProgramSpec {
            kind: Some(if cfg!(windows) { "cmd" } else { "sh" }.to_owned()),
            ..ProgramSpec::default()
        }),
        path: std::env::var("PATH").ok(),
        ..TerminalOpen::default()
    };
    contract::terminal(native.get(), open, &ContractSettings::default());
}

#[cfg(unix)]
mod serial {
    use std::fs::OpenOptions;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Child, Command, Stdio};

    use super::*;

    /// `socat` joining two pseudo-terminals as a null-modem cable; `None` when it is not installed.
    struct Cable {
        child: Child,
        near: String,
        far: String,
    }

    impl Cable {
        fn make() -> Option<Self> {
            let mut child = Command::new("socat")
                .args(["-d", "-d", "pty,raw,echo=0", "pty,raw,echo=0"])
                .stderr(Stdio::piped())
                .spawn()
                .ok()?;
            let stderr = child.stderr.take()?;
            let mut paths = Vec::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(at) = line.find("PTY is ") {
                    paths.push(line[at + 7..].trim().to_owned());
                }
                if paths.len() == 2 {
                    break;
                }
            }
            let far = paths.pop()?;
            let near = paths.pop()?;
            Some(Self { child, near, far })
        }
    }

    impl Drop for Cable {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    fn the_far_end_of_a_serial_cable_is_the_console() {
        let Some(cable) = Cable::make() else {
            println!("SKIPPED: socat is not installed");
            return;
        };
        let source = format!(
            "[source.console]\nprovider = \"serial\"\npath = \"{}\"\nreopen_after = \"200ms\"\n",
            cable.near
        );
        let world = world_serial(&source);
        let mut far = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&cable.far)
            .expect("the far end opens");
        let shell = console(&world);
        far.write_all(b"login: ").expect("the far end writes");
        heard(&shell, "login: ");
        shell.write(b"root\r").expect("typing");
        let mut got = Vec::new();
        let mut byte = [0_u8; 1];
        let until = Instant::now() + WAIT;
        while !got.ends_with(b"root\r") && Instant::now() < until {
            if far.read(&mut byte).unwrap_or(0) == 1 {
                got.push(byte[0]);
            }
        }
        assert!(got.ends_with(b"root\r"), "the far end read {got:?}");
    }

    #[test]
    fn the_serial_source_passes_the_contract_and_returns_after_its_device_vanishes() {
        use bohio_api::contract::{self, ContractSettings};
        use bohio_api::model::TerminalOpen;
        if Command::new("socat").arg("-V").output().is_err() {
            println!("SKIPPED: socat is not installed");
            return;
        }
        let dir = tempfile::tempdir().expect("a directory");
        let link = dir.path().join("line");
        let start = |link: &std::path::Path| -> Child {
            let child = Command::new("socat")
                .arg("pty,raw,echo=0,link=".to_owned() + &link.to_string_lossy())
                .arg("pty,raw,echo=0")
                .spawn()
                .expect("socat starts");
            let until = Instant::now() + WAIT;
            while !link.exists() && Instant::now() < until {
                thread::sleep(Duration::from_millis(20));
            }
            child
        };
        let mut cable = start(&link);
        let config = bohio::config::Config::from_toml(
            &format!(
                "[source.line]
provider = \"serial\"
path = \"{}\"
reopen_after = \"200ms\"
[access]
terminal = [\"line\"]
",
                link.display()
            ),
            "bohio.cfg",
        )
        .expect("a valid configuration")
        .node();
        let sources = Sources::from_config(&config.source, &config.access).expect("sources");
        let line = sources.terminal(None).expect("the line");
        contract::terminal(
            line.get(),
            TerminalOpen::default(),
            &ContractSettings::default(),
        );
        assert!(line.get().describe().expect("describe").available);

        // The cable goes: the source says so at the next `info` and not before, and is back when the device returns.
        let _ = cable.kill();
        let _ = cable.wait();
        let until = Instant::now() + Duration::from_secs(5);
        let gone = loop {
            let info = line.get().describe().expect("describe");
            if !info.available || Instant::now() > until {
                break info;
            }
            thread::sleep(Duration::from_millis(50));
        };
        assert!(!gone.available, "an unplugged line is unavailable");
        assert!(gone.reason.is_some());
        let _ = std::fs::remove_file(&link);
        let mut cable = start(&link);
        let back = Instant::now() + Duration::from_secs(1) + Duration::from_millis(200);
        let mut again = false;
        while Instant::now() < back + Duration::from_secs(2) {
            if line.get().describe().expect("describe").available {
                again = true;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = cable.kill();
        let _ = cable.wait();
        assert!(again, "the line is back within reopen_after plus a second");
    }

    fn world_serial(source: &str) -> World {
        // The serial provider is a built-in of the node, so its settings come from the configuration.
        let config: bohio::config::Config = bohio::config::Config::from_toml(
            &format!("{source}[access]\nterminal = [\"console\"]\ndesktop = []\n"),
            "bohio.cfg",
        )
        .expect("a valid configuration");
        let node_config = config.node();
        let sources =
            Sources::from_config(&node_config.source, &node_config.access).expect("sources");
        let _ = &node_config;
        let world = world(&fake("unused-serial"), granted());
        world
            .host
            .sources()
            .reconfigure(&node_config.source, &node_config.access)
            .expect("serial source");
        drop(sources);
        world
    }
}

#[test]
fn the_default_console_is_named_by_the_host_not_guessed_by_the_client() {
    let _first = testing::terminal_link("console-first-default");
    let _second = testing::terminal_link("console-spare-default");
    let both = format!(
        "{}[source.spare]\nprovider = \"fake_terminal\"\nlink = \"console-spare-default\"\n",
        fake("console-first-default")
    );
    let world = world_with(&both, &["spare", "console"], granted());
    assert_eq!(
        Shell::console_session(&world.link, None, Some(WAIT)).unwrap(),
        "console:spare"
    );
    assert_eq!(
        Shell::console_session(&world.link, Some("console"), Some(WAIT)).unwrap(),
        "console:console"
    );
}
