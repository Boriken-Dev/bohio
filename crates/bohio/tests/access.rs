//! Sources and `[access]`: what a node serves is decided by the sources it holds, and `native` is one of them.

#![forbid(unsafe_code)]

use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bohio::access::Sources;
use bohio::config::{AccessConfig, Config, ConfigError, GuiConfig, NodeConfig, SourceTables};
use bohio::node::{MemoryGrants, MemoryKeys, Node, PeerLink};
use bohio::policy::{Grant, Permission};
use bohio_api::testing;
use bohio_client::Error;
use bohio_client::control::Control;
use bohio_client::gui::Gui;
use bohio_client::link::{Settings, stream};
use bohio_client::shell::Shell;
use bohio_proto::control::Call;
use bohio_proto::gui::{DisplayId, GuiRequest, Want};
use bohio_proto::shell::{Request, ShellKind};
use bohio_proto::{ErrorBody, ErrorCode};
use cabuya::auth::{Identity, Method};
use coqui_peer::PeerId;
use coqui_transport::{Channel, StreamChannel, memory};

const WAIT: Duration = Duration::from_secs(30);

fn invalid(text: &str) -> (String, String) {
    match Config::from_toml(text, "bohio.cfg") {
        Err(ConfigError::Invalid { key, message, .. }) => (key, message),
        other => panic!("expected an invalid configuration, got {other:?}"),
    }
}

#[test]
fn a_refused_source_or_access_table_names_its_key() {
    let (key, message) = invalid("[source.x]\nprovider = \"no-such-provider\"\n");
    assert_eq!(key, "source.x.provider");
    assert!(
        message.contains("native"),
        "the known providers are listed: {message}"
    );

    let (key, _) = invalid("[source.x]\n");
    assert_eq!(key, "source.x");

    let (key, _) = invalid("[source.native]\nprovider = \"fake_terminal\"\n");
    assert_eq!(key, "source.native.provider");

    let (key, message) = invalid("[source.native]\nbogus = 1\n");
    assert_eq!(key, "source.native.bogus");
    assert!(
        message.contains("read_chunk"),
        "the provider's keys are listed: {message}"
    );

    let (key, _) = invalid("[source.native]\nread_chunk = \"many\"\n");
    assert_eq!(key, "source.native.read_chunk");

    let (key, _) = invalid("[source.native]\nread_chunk = 0\n");
    assert_eq!(key, "source.native");
    let (key, _) = invalid("[source.native]\nwindows_capture = \"magic\"\n");
    assert_eq!(key, "source.native");

    // The Linux desktop's keys are checked on every platform, so a file is the same wherever it is read.
    for bad in [
        "x11_max_repeat = 0",
        "x11_bus_wait = \"0s\"",
        "x11_key_wait = \"soon\"",
        "x11_selection = \"\"",
        "macos_capture_wait = \"0s\"",
        "macos_capture_wait = \"soon\"",
        "macos_max_repeat = 0",
    ] {
        let (key, _) = invalid(&format!("[source.native]\n{bad}\n"));
        assert_eq!(key, "source.native", "{bad}");
    }

    let (key, _) = invalid("[access]\nterminal = [\"ghost\"]\n");
    assert_eq!(key, "access.terminal");
    let (key, _) = invalid("[access]\npower = [\"native\"]\n");
    assert_eq!(key, "access.power");

    // Nothing in the tables of today's node is refused.
    Config::from_toml(
        "[source.native]\nread_chunk = 4096\nwindows_capture = \"gdi\"\nx11_display = \":0\"\nx11_in_wayland = true\n[access]\nterminal = [\"native\"]\nfs = \"none\"\n",
        "bohio.cfg",
    )
    .expect("a valid configuration");
}

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
    _wire: Wire,
}

/// A host with `sources` (or today's) and a client granted `grant`, linked over memory pipes.
fn world(config: NodeConfig, sources: Option<Sources>, grant: Grant) -> World {
    let make = |config: NodeConfig, sources: Option<Sources>| {
        let identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let id = identity.peer_id();
        let keys = Arc::new(MemoryKeys::new());
        let grants = Arc::new(MemoryGrants::new());
        let mut node = Node::new(config, identity, keys.clone(), grants.clone())
            .expect("the node is built")
            .with_path(std::env::var_os("PATH").unwrap_or_default());
        if let Some(sources) = sources {
            node = node.with_sources(sources);
        }
        (Arc::new(node), id, keys, grants)
    };
    let (host, host_id, host_keys, host_grants) = make(config, sources);
    let (client, client_id, client_keys, _) = make(NodeConfig::default(), None);
    host_keys.accept(client_id);
    host_grants.set(client_id, grant);
    client_keys.accept(host_id);
    let settings = Settings::default();
    let (one, two) = memory::pipe(1024 * 1024);
    let dial_end = Arc::new(stream(one, &settings).expect("a channel"));
    let answer_end = Arc::new(stream(two, &settings).expect("a channel"));
    let mut threads = Vec::new();
    threads.push(thread::spawn({
        let (node, channel) = (client.clone(), dial_end.clone());
        move || {
            let _ = node.connect_channel(&*channel, &host_id, None);
            channel.close();
        }
    }));
    threads.push(thread::spawn({
        let (node, channel) = (host.clone(), answer_end.clone());
        move || {
            node.serve_channel(&*channel, None);
            channel.close();
        }
    }));
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
        _wire: Wire {
            channels: [dial_end, answer_end],
            threads,
        },
    }
}

fn remote(error: &Error) -> &ErrorBody {
    match error {
        Error::Remote(body) => body,
        other => panic!("expected the host's error, got {other:?}"),
    }
}

fn node_config(toml: &str) -> NodeConfig {
    Config::from_toml(toml, "bohio.cfg")
        .expect("a valid configuration")
        .node()
}

#[test]
fn access_fs_none_makes_every_file_call_unsupported() {
    let dir = tempfile::tempdir().expect("a directory");
    let grant = Grant::allowing([Permission::Info, Permission::FsRead])
        .with_fs_roots([dir.path().to_path_buf()]);
    let stat = |world: &World| {
        let control = Control::open(&world.link).expect("control opens");
        control.call(
            Call::Stat {
                path: dir.path().to_string_lossy().into_owned(),
                follow: false,
            },
            Some(WAIT),
        )
    };
    let served = world(NodeConfig::default(), None, grant.clone());
    assert!(stat(&served).is_ok(), "today's node serves files");
    let none = world(node_config("[access]\nfs = \"none\"\n"), None, grant);
    let refused = stat(&none).expect_err("no files are served");
    assert_eq!(remote(&refused).code, ErrorCode::Unsupported);
}

fn shell_request() -> Request {
    #[cfg(unix)]
    return Request::kind(ShellKind::Sh);
    #[cfg(windows)]
    return Request::kind(ShellKind::Cmd);
}

#[test]
fn an_empty_terminal_list_refuses_shells_although_the_platform_has_a_pty() {
    let grant = Grant::allowing([Permission::Shell]);
    let with = world(NodeConfig::default(), None, grant.clone());
    let shell = Shell::start(&with.link, &shell_request(), Some(WAIT)).expect("native serves it");
    shell.kill().ok();
    drop(shell);

    let without = world(node_config("[access]\nterminal = []\n"), None, grant);
    let error = Shell::start(&without.link, &shell_request(), Some(WAIT)).map(drop);
    match error {
        Err(error) => assert_eq!(remote(&error).code, ErrorCode::Unsupported),
        Ok(()) => panic!("a node with no terminal source opened a shell"),
    }
}

#[test]
fn info_says_no_shell_is_served_when_access_names_no_terminal() {
    use bohio_proto::control::Reply;
    let info_of = |world: &World| match Control::open(&world.link)
        .expect("control opens")
        .call(Call::Info, Some(WAIT))
        .expect("info")
    {
        Reply::Info(info) => info,
        other => panic!("not info: {other:?}"),
    };
    let grant = Grant::allowing([Permission::Info]);
    let with = info_of(&world(NodeConfig::default(), None, grant.clone()));
    assert!(with.shell.enabled && !with.shells.is_empty());
    let without = info_of(&world(
        node_config(
            "[access]
terminal = []
",
        ),
        None,
        grant,
    ));
    assert!(!without.shell.enabled, "{:?}", without.shell);
    assert!(without.shells.is_empty(), "{:?}", without.shells);
}

fn tables(text: &str) -> SourceTables {
    #[derive(serde::Deserialize)]
    struct Whole {
        source: SourceTables,
    }
    toml::from_str::<Whole>(text).expect("source tables").source
}

fn fake_sources(text: &str) -> Sources {
    let sources = Sources::unconfigured();
    sources
        .register_library("bohio-api-testing", testing::library)
        .expect("the fake providers register");
    sources
        .reconfigure(
            &tables(text),
            &AccessConfig {
                desktop: Some(vec!["screen".to_owned()]),
                terminal: Some(Vec::new()),
                ..AccessConfig::default()
            },
        )
        .expect("the sources build");
    sources
}

#[test]
fn a_reload_that_changes_a_source_ends_the_stream_running_on_it() {
    let gui = GuiConfig {
        enabled: true,
        ..GuiConfig::default()
    };
    let config = NodeConfig {
        gui,
        ..NodeConfig::default()
    };
    let one = "[source.screen]\nprovider = \"fake_desktop\"\nlink = \"access-reload\"\n";
    let grant = Grant::allowing([Permission::Info, Permission::GuiView]);
    let world = world(config, Some(fake_sources(one)), grant);
    let session = Gui::open(&world.link, &GuiRequest::new([Want::View])).expect("the stream opens");
    session
        .snapshot(&snapshot_options(), Some(WAIT))
        .expect("a snapshot of the fake screen");

    // The same table again changes nothing: the stream goes on.
    world
        .host
        .sources()
        .reconfigure(
            &tables(one),
            &AccessConfig {
                desktop: Some(vec!["screen".to_owned()]),
                terminal: Some(Vec::new()),
                ..AccessConfig::default()
            },
        )
        .expect("the same sources");
    session
        .snapshot(&snapshot_options(), Some(WAIT))
        .expect("an unchanged source keeps its streams");

    // A changed table builds a new instance and retires the old one.
    let two = "[source.screen]\nprovider = \"fake_desktop\"\nlink = \"access-reload-two\"\n";
    world
        .host
        .sources()
        .reconfigure(
            &tables(two),
            &AccessConfig {
                desktop: Some(vec!["screen".to_owned()]),
                terminal: Some(Vec::new()),
                ..AccessConfig::default()
            },
        )
        .expect("the changed sources");
    let ended = session
        .snapshot(&snapshot_options(), Some(WAIT))
        .expect_err("the stream runs on a retired source");
    let body = remote(&ended);
    assert_eq!(body.code, ErrorCode::Unavailable);
    assert!(body.message.contains("reconfigured"), "{}", body.message);
}

fn snapshot_options() -> bohio_client::gui::Snapshot {
    bohio_client::gui::Snapshot {
        display: Some(DisplayId("fake-1".to_owned())),
        ..bohio_client::gui::Snapshot::default()
    }
}

#[test]
fn native_is_one_source_among_others_and_the_default_changes_with_access() {
    let sources =
        fake_sources("[source.screen]\nprovider = \"fake_desktop\"\nlink = \"access-order\"\n");
    assert_eq!(
        sources.names(bohio::access::Capability::Desktop),
        ["screen"]
    );
    assert!(sources.desktop(None).is_ok());
    assert_eq!(
        sources.terminal(None).expect_err("no terminal source").code,
        ErrorCode::Unsupported
    );
    assert_eq!(
        sources
            .desktop(Some("native"))
            .expect("native is a source too, listed or not")
            .name(),
        "native"
    );
    let _: PeerId = Identity::generate(Method::DEFAULT).expect("id").peer_id();
}

struct Still;

impl bohio_api::PowerSource for Still {
    fn describe(
        &self,
    ) -> Result<bohio_api::SourceInfo, bohio_api::guatiao::library::ProviderError> {
        Ok(bohio_api::SourceInfo::default())
    }

    fn state(&self) -> Result<bohio_api::PowerState, bohio_api::guatiao::library::ProviderError> {
        Ok(bohio_api::PowerState {
            state: "on".to_owned(),
            done: true,
        })
    }

    fn act(
        &self,
        _action: &str,
    ) -> Result<bohio_api::PowerState, bohio_api::guatiao::library::ProviderError> {
        self.state()
    }
}

#[test]
fn every_capability_has_a_with_and_an_add_for_an_embedders_source() {
    let sources = Sources::unconfigured();
    sources.add_power("mine", Arc::new(Still));
    assert!(sources.power(Some("mine")).is_ok());
    let sources = Sources::unconfigured()
        .with_power("again", Arc::new(Still))
        .with_access(bohio::access::Capability::Power, ["again"]);
    assert_eq!(
        sources.names(bohio::access::Capability::Power),
        ["again".to_owned()]
    );
    assert!(sources.power(None).is_ok());
}
