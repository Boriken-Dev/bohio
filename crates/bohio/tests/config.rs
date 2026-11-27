//! `bohio.cfg`: defaults, refusals, the layers and the peers they hold, and the bounds.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use bohio::config::{Config, ConfigError, Environment, Layers, ServiceConfig, Span};
use bohio::policy::Permission;
use cabuya::auth::{Identity, Method};

fn example() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/bohio.cfg")
}

fn parse(text: &str) -> Result<Config, ConfigError> {
    Config::from_toml(text, "bohio.cfg")
}

/// A peer id, as `bohio id` prints one.
fn peer_id() -> String {
    Identity::generate(Method::DEFAULT)
        .expect("an identity")
        .peer_id()
        .to_string()
}

/// A scratch directory with `files` written in it, in order; returns their paths.
fn layers(dir: &Path, files: &[(&str, &str)]) -> Vec<PathBuf> {
    files
        .iter()
        .map(|(name, text)| {
            let path = dir.join(name);
            std::fs::write(&path, text).unwrap();
            path
        })
        .collect()
}

#[test]
fn the_example_configuration_is_accepted_and_states_every_default() {
    let config = Config::load(&example()).expect("the example loads");
    // The example writes every default out: it must equal the built-in ones, except for the
    // listener it demonstrates.
    let defaults = Config {
        listen: config.listen.clone(),
        ..Config::default()
    };
    assert_eq!(config, defaults);
    assert_eq!(config.listen[0].endpoint, "tcp://127.0.0.1:7620");
}

#[test]
fn an_empty_file_is_the_defaults() {
    assert_eq!(parse("").unwrap(), Config::default());
}

#[test]
fn an_unknown_key_is_refused_by_name_with_its_position() {
    let error = parse("[execution]\nmax_procesess = 4\n").unwrap_err();
    let ConfigError::Parse { line, message, .. } = &error else {
        panic!("expected a parse error, got {error:?}");
    };
    assert_eq!(*line, 2);
    assert!(message.contains("max_procesess"), "{message}");
    assert!(error.to_string().starts_with("bohio.cfg:2:"), "{error}");
}

#[test]
fn an_unknown_section_and_a_misspelled_listener_key_are_refused() {
    assert!(parse("[nope]\n").unwrap_err().to_string().contains("nope"));
    let error = parse("[[listen]]\nendpint = \"tcp://x:1\"\n").unwrap_err();
    assert!(matches!(error, ConfigError::Parse { .. }), "{error:?}");
}

#[test]
fn a_wrong_type_is_a_parse_error_not_a_default() {
    assert!(matches!(
        parse("[link]\nmax_streams = \"many\"\n"),
        Err(ConfigError::Parse { .. })
    ));
}

#[test]
fn what_a_dead_host_does_to_its_programs_is_kill_term_or_leave() {
    use bohio::config::HostDeath;
    assert_eq!(parse("").unwrap().execution.host_death, HostDeath::Kill);
    for (text, want) in [
        ("kill", HostDeath::Kill),
        ("term", HostDeath::Term),
        ("leave", HostDeath::Leave),
    ] {
        let config = parse(&format!(
            "[execution]
host_death = \"{text}\"
"
        ))
        .unwrap();
        assert_eq!(config.execution.host_death, want, "{text}");
    }
    assert!(
        parse(
            "[execution]
host_death = \"pause\"
"
        )
        .is_err()
    );
}

#[test]
fn times_take_seconds_or_a_unit() {
    let config = parse(
        "[link]\nrecheck = \"250ms\"\n[execution]\npoll_interval = 2\ndrain_timeout = \"1m\"\n",
    )
    .unwrap();
    assert_eq!(config.link.recheck, Span(Duration::from_millis(250)));
    assert_eq!(config.execution.poll_interval, Span(Duration::from_secs(2)));
    assert_eq!(
        config.execution.drain_timeout,
        Span(Duration::from_secs(60))
    );
    assert!(parse("[link]\nrecheck = \"soon\"\n").is_err());
    assert!(parse("[link]\nrecheck = -1\n").is_err());
}

#[test]
fn values_out_of_range_name_their_key() {
    for (text, key) in [
        ("[link]\nmax_streams = 0\n", "link.max_streams"),
        ("[link]\nrecheck = 0\n", "link.recheck"),
        ("[link]\nwindow = 0\n", "link.window"),
        ("[link]\nqueue_bytes = 0\n", "link.queue_bytes"),
        ("[link]\nread_ahead = 0\n", "link.read_ahead"),
        ("[link]\nmax_connections = 0\n", "link.max_connections"),
        (
            "[link]\nmax_unproven_per_source = 0\n",
            "link.max_unproven_per_source",
        ),
        (
            "[link]\nmax_links_per_peer = 0\n",
            "link.max_links_per_peer",
        ),
        ("[link]\nproof_timeout = 0\n", "link.proof_timeout"),
        ("[pairing]\ntimeout = 0\n", "pairing.timeout"),
        ("[pairing]\npin_length = 0\n", "pairing.pin_length"),
        ("[pairing]\npending = 0\n", "pairing.pending"),
        (
            "[pairing]\npending_per_source = 0\n",
            "pairing.pending_per_source",
        ),
        ("[pairing]\nipv6_prefix = 0\n", "pairing.ipv6_prefix"),
        ("[pairing]\nipv6_prefix = 129\n", "pairing.ipv6_prefix"),
        (
            "[pairing]\nexchange_timeout = 0\n",
            "pairing.exchange_timeout",
        ),
        ("[link]\naccept_wait = 0\n", "link.accept_wait"),
        ("[link]\naccept_poll = 0\n", "link.accept_poll"),
        ("[service]\nstop_wait = 0\n", "service.stop_wait"),
        ("[waiting]\nper_source = 0\n", "waiting.per_source"),
        ("[waiting]\ntotal = 0\n", "waiting.total"),
        ("[waiting]\nipv6_prefix = 0\n", "waiting.ipv6_prefix"),
        ("[waiting]\nipv6_prefix = 129\n", "waiting.ipv6_prefix"),
        ("[client]\nreply_timeout = 0\n", "client.reply_timeout"),
        ("[client]\ngui_timeout = 0\n", "client.gui_timeout"),
        ("[client]\ninput_chunk = 0\n", "client.input_chunk"),
        ("[client]\nread_size = 0\n", "client.read_size"),
        ("[client]\ninput_queue = 0\n", "client.input_queue"),
        ("[client]\nprobe_timeout = 0\n", "client.probe_timeout"),
        ("[client]\nlinks_wait = 0\n", "client.links_wait"),
        (
            "[client]\nparallel_streams = 0\n",
            "client.parallel_streams",
        ),
        ("[client]\nparallel_links = 0\n", "client.parallel_links"),
        ("[client]\npower_wait = 0\n", "client.power_wait"),
        ("[client]\nkill_after = 0\n", "client.kill_after"),
        ("[client]\nsync_bundle_max = 0\n", "client.sync_bundle_max"),
        (
            "[client]\nsync_bundle_items = 0\n",
            "client.sync_bundle_items",
        ),
        ("[mcp]\nmax_calls = 0\n", "mcp.max_calls"),
        (
            "[pairing]\nattempt_delay = \"5s\"\nattempt_delay_max = \"1s\"\n",
            "pairing.attempt_delay_max",
        ),
        ("[link]\nmax_message = 4096\n", "link.max_message"),
        (
            "[execution]\nmax_processes = 0\n",
            "execution.max_processes",
        ),
        ("[execution]\nchunk_size = 0\n", "execution.chunk_size"),
        (
            "[execution]\npoll_interval = 0\n",
            "execution.poll_interval",
        ),
        (
            "[filesystem]\nmax_transfers = 0\n",
            "filesystem.max_transfers",
        ),
        ("[filesystem]\nchunk_size = 0\n", "filesystem.chunk_size"),
        ("[filesystem]\nlist_page = 0\n", "filesystem.list_page"),
        (
            "[filesystem]\nidle_timeout = 0\n",
            "filesystem.idle_timeout",
        ),
        (
            "[filesystem]\npoll_interval = 0\n",
            "filesystem.poll_interval",
        ),
        ("[hash]\nmax_streams = 0\n", "hash.max_streams"),
        ("[hash]\nread_size = 0\n", "hash.read_size"),
        ("[hash]\nalgorithms = []\n", "hash.algorithms"),
        ("[hash]\nalgorithms = [\"whirlpool\"]\n", "hash.algorithms"),
        (
            "[hash]\nalgorithms = [\"md5\"]\n",
            "filesystem.write_hashes",
        ),
        (
            "[compression]\nallowed = [\"brotli\"]\n",
            "compression.allowed",
        ),
        (
            "[compression]\ndefault = { algorithm = \"none\" }\n",
            "compression.default",
        ),
        (
            "[compression]\nallowed = [\"lz4\"]\ndefault = { algorithm = \"deflate\" }\n",
            "compression.default",
        ),
        (
            "[compression]\nwindow_log_max = 5\n",
            "compression.window_log_max",
        ),
        (
            "[compression]\nwindow_log_max = 40\n",
            "compression.window_log_max",
        ),
        (
            "[compression]\ndeflate_max_level = 12\n",
            "compression.deflate_max_level",
        ),
        ("[compression]\nmax_level = 0\n", "compression.max_level"),
        (
            "[compression]\nsample_ratio = 0.0\n",
            "compression.sample_ratio",
        ),
        (
            "[compression]\nprepare_max_bytes = 0\n",
            "compression.prepare_max_bytes",
        ),
        (
            "[compression]\nprepare_ttl = 0\n",
            "compression.prepare_ttl",
        ),
        ("[compression]\nexclude = [\"[a\"]\n", "compression.exclude"),
        ("[delta]\nmin_block = 0\n", "delta.min_block"),
        (
            "[delta]\nmin_block = 4096\nmax_block = 2048\n",
            "delta.max_block",
        ),
        ("[delta]\nmax_block = 2147483648\n", "delta.max_block"),
        (
            "[delta]\nmax_signature_bytes = 0\n",
            "delta.max_signature_bytes",
        ),
        ("[walk]\nmax_streams = 0\n", "walk.max_streams"),
        ("[shell]\nmax_sessions = 0\n", "shell.max_sessions"),
        ("[shell]\nchunk_size = 0\n", "shell.chunk_size"),
        ("[shell]\npoll_interval = 0\n", "shell.poll_interval"),
        (
            "[forward]\nmax_connections = 0\n",
            "forward.max_connections",
        ),
        (
            "[forward]\nmax_connections_per_peer = 0\n",
            "forward.max_connections_per_peer",
        ),
        ("[forward]\nchunk_size = 0\n", "forward.chunk_size"),
        (
            "[forward]\nconnect_timeout = 0\n",
            "forward.connect_timeout",
        ),
        ("[forward]\nstalled = 0\n", "forward.stalled"),
        ("[forward]\nwatch = 0\n", "forward.watch"),
        ("[forward]\nlisten_poll = 0\n", "forward.listen_poll"),
        ("[forward]\nanswer_timeout = 0\n", "forward.answer_timeout"),
        (
            "[forward]\nmax_local_connections = 0\n",
            "forward.max_local_connections",
        ),
        ("[forward]\nmax_listeners = 0\n", "forward.max_listeners"),
        (
            "[forward]\nmax_listeners_per_peer = 0\n",
            "forward.max_listeners_per_peer",
        ),
        ("[forward]\nmax_pending = 0\n", "forward.max_pending"),
        ("[forward]\naccept_timeout = 0\n", "forward.accept_timeout"),
        (
            "[forward]\nidle_min = \"5ms\"\nidle_max = \"1ms\"\n",
            "forward.idle_max",
        ),
        ("[forward]\nchunk_size = 2000000\n", "link.max_message"),
        ("[[listen]]\nendpoint = \"nowhere\"\n", "listen.endpoint"),
    ] {
        match parse(text) {
            Err(ConfigError::Invalid { key: found, .. }) => assert_eq!(found, key, "{text}"),
            other => panic!("{text}: expected Invalid, got {other:?}"),
        }
    }
}

#[test]
fn an_error_never_quotes_the_line_it_found() {
    let error = parse("[service]\nname = 12345-a-secret-looking-value\n").unwrap_err();
    assert!(!error.to_string().contains("secret-looking"), "{error}");
}

fn environment(windows: bool) -> Environment {
    Environment {
        windows,
        local_app_data: Some("L".into()),
        xdg_data_home: Some("D".into()),
        home: Some("H".into()),
        ..Environment::default()
    }
}

#[test]
fn the_data_directory_defaults_under_the_users_data_directory() {
    assert_eq!(
        environment(true).default_data_dir(),
        Path::new("L").join("bohio")
    );
    assert_eq!(
        environment(false).default_data_dir(),
        Path::new("D").join("bohio")
    );
    let mut config = Config::default();
    assert_eq!(
        config.data_dir(&environment(false)),
        Path::new("D").join("bohio")
    );
    config.service.data_dir = Some("/var/lib/bohio".into());
    assert_eq!(
        config.data_dir(&environment(false)),
        Path::new("/var/lib/bohio")
    );
}

#[test]
fn each_peer_table_reads_as_that_peers_grant() {
    let (laptop, guest) = (peer_id(), peer_id());
    let config = parse(&format!(
        r#"
[peer."{laptop}"]
allow = ["info", "fs.read", "exec"]
fs_roots = ["/srv/data"]

[peer."{laptop}".exec]
programs = ["git", "make"]
env = ["LANG"]

[peer."{guest}"]
allow = ["info"]
"#
    ))
    .unwrap();
    let policies = config.peer;
    let laptop = policies.grant_for(&laptop);
    assert!(laptop.allows(Permission::Exec) && laptop.allows(Permission::FsRead));
    assert!(!laptop.allows(Permission::FsWrite) && !laptop.allows(Permission::Shell));
    assert_eq!(laptop.fs_roots(), [PathBuf::from("/srv/data")]);
    assert!(laptop.permits_program("git") && !laptop.permits_program("rm"));
    assert!(!laptop.permits_shell_script());
    assert_eq!(laptop.env_names(), ["LANG"]);
    let mut lang = bohio_proto::exec::Env::default();
    lang.set.insert("LANG".to_owned(), "C".to_owned());
    assert!(laptop.permits_env(&lang));
    lang.set.insert("LD_PRELOAD".to_owned(), "x".to_owned());
    assert!(!laptop.permits_env(&lang));
    let guest = policies.grant_for(&guest);
    assert!(guest.allows(Permission::Info) && !guest.allows(Permission::Exec));
    assert!(guest.permits_program("anything") && guest.permits_shell_script());
    assert!(
        guest.permits_env(&lang),
        "without a program list any variable may be set"
    );
    assert_eq!(policies.len(), 2);
}

#[test]
fn an_empty_table_accepts_a_key_and_an_unlisted_key_gets_nothing() {
    let quiet = peer_id();
    let config = parse(&format!("[peer.\"{quiet}\"]\n")).unwrap();
    assert!(config.peer.has(&quiet));
    assert!(!config.peer.has("stranger"));
    for id in [quiet.as_str(), "stranger"] {
        let grant = config.peer.grant_for(id);
        assert_eq!(grant.permissions().count(), 0, "{id}");
        assert!(grant.fs_roots().is_empty());
    }
}

#[test]
fn an_unknown_permission_is_refused_naming_its_line() {
    let id = peer_id();
    for permission in ["root", "fs.rw", "Exec", ""] {
        let text = format!("[peer.\"{id}\"]\nallow = [\"{permission}\"]\n");
        let error = parse(&text).unwrap_err();
        let ConfigError::Parse { line, key, .. } = &error else {
            panic!("expected a parse error for {permission:?}, got {error:?}");
        };
        assert_eq!(*line, 2, "{error}");
        assert!(key.is_some(), "{error}");
        assert!(error.to_string().contains("unknown variant"), "{error}");
    }
}

#[test]
fn a_peer_table_refuses_unknown_keys() {
    let id = peer_id();
    for body in [
        "allow = []\nfs_root = []\n",
        "[peer.\"ID\".exec]\nprogram = []\n",
    ] {
        let text = format!("[peer.\"{id}\"]\n{}", body.replace("ID", &id));
        assert!(parse(&text).is_err(), "{text}");
    }
    assert!(parse("[peers]\n").is_err());
}

#[test]
fn a_table_name_that_is_not_a_peer_id_is_refused_naming_the_key() {
    match parse("[peer.laptop]\nallow = [\"info\"]\n") {
        Err(ConfigError::Invalid { key, path, .. }) => {
            assert_eq!(key, "peer.laptop");
            assert_eq!(path, "bohio.cfg");
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn every_permission_name_round_trips_through_the_file() {
    let id = peer_id();
    for permission in [
        Permission::Info,
        Permission::FsRead,
        Permission::FsWrite,
        Permission::Exec,
        Permission::Shell,
        Permission::Admin,
        Permission::ForwardConnect,
        Permission::GuiView,
        Permission::GuiInput,
        Permission::GuiClipboard,
    ] {
        let text = format!("[peer.\"{id}\"]\nallow = [\"{}\"]\n", permission.name());
        assert!(parse(&text).unwrap().peer.grant_for(&id).allows(permission));
    }
}

#[test]
fn layers_merge_tables_by_key_and_replace_arrays() {
    let dir = tempfile::tempdir().unwrap();
    let id = peer_id();
    let files = layers(
        dir.path(),
        &[
            (
                "system.cfg",
                &format!(
                    "[link]\nrecheck = \"7s\"\nmax_streams = 10\n\n[[listen]]\nendpoint = \"tcp://127.0.0.1:1\"\n\n[[listen]]\nendpoint = \"tcp://127.0.0.1:2\"\n\n[peer.\"{id}\"]\nallow = [\"info\", \"exec\"]\nfs_roots = [\"/a\", \"/b\"]\n"
                ),
            ),
            (
                "user.cfg",
                &format!(
                    "[link]\nmax_streams = 20\n\n[[listen]]\nendpoint = \"tcp://127.0.0.1:3\"\n\n[peer.\"{id}\"]\nfs_roots = [\"/c\"]\n"
                ),
            ),
        ],
    );
    let layered = Layers::load(&files).unwrap();
    let config = &layered.config;
    assert_eq!(
        config.link.recheck,
        Span(Duration::from_secs(7)),
        "a table's other keys stay"
    );
    assert_eq!(config.link.max_streams, 20, "a scalar is replaced");
    let listen: Vec<&str> = config.listen.iter().map(|l| l.endpoint.as_str()).collect();
    assert_eq!(
        listen,
        ["tcp://127.0.0.1:3"],
        "an array is replaced, not merged"
    );
    let grant = layered.peers.grant_for(&id);
    assert!(grant.allows(Permission::Info) && grant.allows(Permission::Exec));
    assert_eq!(
        grant.fs_roots(),
        [PathBuf::from("/c")],
        "a higher layer narrows a root list"
    );
    assert_eq!(layered.files, files);
}

#[test]
fn a_peer_is_accepted_while_any_layer_holds_its_table() {
    let dir = tempfile::tempdir().unwrap();
    let (low, high) = (peer_id(), peer_id());
    let files = layers(
        dir.path(),
        &[
            ("a.cfg", &format!("[peer.\"{low}\"]\n")),
            ("b.cfg", &format!("[peer.\"{high}\"]\nallow = [\"info\"]\n")),
            ("missing.cfg", ""),
        ],
    );
    std::fs::remove_file(&files[2]).unwrap();
    let peers = Layers::peers(&files).unwrap();
    assert!(peers.has(&low) && peers.has(&high));
    assert!(peers.grant_for(&high).allows(Permission::Info));
    assert!(
        Layers::peers(&[dir.path().join("nope.cfg")])
            .unwrap()
            .is_empty(),
        "a layer that does not exist yet is skipped"
    );
}

/// `[filesystem] allow_id_bits` lets a peer make a file that runs as the service's account: only the lowest layer sets it.
#[test]
fn allow_id_bits_set_above_the_lowest_layer_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let files = layers(
        dir.path(),
        &[
            (
                "system.cfg",
                "[filesystem]
max_transfers = 4
",
            ),
            (
                "user.cfg",
                "[filesystem]
allow_id_bits = true
",
            ),
        ],
    );
    assert!(
        !Layers::load(&files)
            .unwrap()
            .config
            .filesystem
            .allow_id_bits
    );
    let files = layers(
        dir.path(),
        &[(
            "low.cfg",
            "[filesystem]
allow_id_bits = true
",
        )],
    );
    assert!(
        Layers::load(&files)
            .unwrap()
            .config
            .filesystem
            .allow_id_bits
    );
}

/// Whoever writes a higher layer must not set the keys that govern layering and weaken checks: the value is ignored, not
/// the configuration refused, unless the lowest layer allows the key.
#[test]
fn a_governing_key_set_above_the_lowest_layer_is_ignored_unless_the_lowest_layer_allows_it() {
    let dir = tempfile::tempdir().unwrap();
    let keys = [
        ("service", "check_access = false"),
        ("service", "user_layer = false"),
        ("secrets", "check_access = false"),
        ("secrets", "group_readable = true"),
        ("secrets", "allow_literal = true"),
        ("providers", "check_access = false"),
        ("gui", "node_account = \"someone\""),
    ];
    for (table, line) in keys {
        let dotted = format!("{table}.{}", line.split(' ').next().unwrap());
        let user = format!("[{table}]\n{line}\n");
        let files = layers(
            dir.path(),
            &[
                ("system.cfg", "[service]\nname = \"s\"\n"),
                ("user.cfg", &user),
            ],
        );
        let layered = Layers::load(&files).unwrap_or_else(|error| panic!("{dotted}: {error}"));
        assert_eq!(
            layered.config.service,
            ServiceConfig {
                name: "s".to_owned(),
                ..ServiceConfig::default()
            },
            "{dotted} was read from a higher layer"
        );
        assert!(layered.config.secrets.check_access && !layered.config.secrets.allow_literal);
        assert!(layered.config.providers.check_access);
        // The lowest layer can allow it.
        let system = format!("[service]\nupper_layer_keys = [\"{dotted}\"]\n");
        let files = layers(dir.path(), &[("system.cfg", &system), ("user.cfg", &user)]);
        let layered = Layers::load(&files).unwrap_or_else(|error| panic!("{dotted}: {error}"));
        let defaults = Layers::load(&layers(
            dir.path(),
            &[("d.cfg", "[service]\nname = \"bohio\"\n")],
        ))
        .unwrap();
        assert_ne!(
            (
                &layered.config.service,
                &layered.config.secrets,
                &layered.config.providers
            ),
            (
                &defaults.config.service,
                &defaults.config.secrets,
                &defaults.config.providers
            ),
            "{dotted} was not read although the lowest layer allows it"
        );
    }
    // The lowest layer may set them.
    let files = layers(
        dir.path(),
        &[(
            "low.cfg",
            "[service]\ncheck_access = false\nuser_layer = false\n",
        )],
    );
    let layered = Layers::load(&files).unwrap();
    assert!(!layered.config.service.check_access && !layered.config.service.user_layer);
    // A name that is no such key cannot work.
    let files = layers(
        dir.path(),
        &[(
            "low.cfg",
            "[service]\nupper_layer_keys = [\"service.name\"]\n",
        )],
    );
    assert!(matches!(
        Layers::load(&files),
        Err(ConfigError::Invalid { key, .. }) if key == "service.upper_layer_keys"
    ));
}

#[test]
fn an_error_names_the_file_line_column_and_key_and_never_the_value() {
    let dir = tempfile::tempdir().unwrap();
    let files = layers(
        dir.path(),
        &[
            ("system.cfg", "[link]\nmax_streams = 8\n"),
            (
                "user.cfg",
                "# a comment\n[link]\nrecheck = \"5s\"\nmax_streams = \"MARKER-VALUE-1234\"\n",
            ),
        ],
    );
    let error = Layers::load(&files).unwrap_err();
    let ConfigError::Parse {
        path,
        line,
        column,
        key,
        ..
    } = &error
    else {
        panic!("expected a parse error, got {error:?}");
    };
    assert!(path.ends_with("user.cfg"), "{path}");
    assert_eq!((*line, *column), (4, 1), "{error}");
    assert_eq!(key.as_deref(), Some("link.max_streams"));
    let shown = format!("{error} {error:?}");
    assert!(!shown.contains("MARKER-VALUE"), "{shown}");

    // An unknown key is named where it is written, and a syntax error where the parser stopped.
    let files = layers(
        dir.path(),
        &[
            ("a.cfg", "[execution]\nfast = true\n"),
            ("b.cfg", "[link\n"),
        ],
    );
    let error = Layers::load(&files[..1]).unwrap_err();
    let ConfigError::Parse {
        line, key, message, ..
    } = &error
    else {
        panic!("{error:?}");
    };
    assert_eq!(*line, 2);
    assert_eq!(key.as_deref(), Some("execution.fast"));
    assert!(message.contains("unknown field"), "{message}");
    let error = Layers::load(&files[1..]).unwrap_err();
    assert!(
        matches!(
            &error,
            ConfigError::Parse {
                line: 1,
                key: None,
                ..
            }
        ),
        "{error:?}"
    );
}

#[test]
fn a_range_error_names_the_highest_layer_that_sets_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let files = layers(
        dir.path(),
        &[
            ("system.cfg", "[link]\nmax_streams = 0\n"),
            ("user.cfg", "[link]\nrecheck = \"3s\"\n"),
        ],
    );
    match Layers::load(&files) {
        Err(ConfigError::Invalid { path, key, .. }) => {
            assert_eq!(key, "link.max_streams");
            assert!(
                path.ends_with("system.cfg"),
                "the layer that set the zero: {path}"
            );
        }
        other => panic!("expected Invalid, got {other:?}"),
    }
    let files = layers(
        dir.path(),
        &[
            ("system.cfg", "[link]\nmax_streams = 4\n"),
            ("user.cfg", "[link]\nmax_streams = 0\n"),
        ],
    );
    match Layers::load(&files) {
        Err(ConfigError::Invalid { path, .. }) => assert!(path.ends_with("user.cfg"), "{path}"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn a_format_line_is_a_comment_and_a_missing_layer_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let files = layers(
        dir.path(),
        &[("a.cfg", "#!/usr/bin/env bohio\n[service]\nname = \"x\"\n")],
    );
    assert_eq!(Layers::load(&files).unwrap().config.service.name, "x");
    assert!(matches!(
        Layers::load(&[dir.path().join("missing.cfg")]),
        Err(ConfigError::Read { .. })
    ));
    assert_eq!(
        Layers::load(&[]).unwrap().config,
        Config::default(),
        "no layer is the defaults"
    );
}

#[test]
fn the_waits_that_were_literals_are_settings_with_their_old_values() {
    let config = parse("").unwrap();
    let settings = config.link.settings();
    assert_eq!(settings.accept_wait, Duration::from_millis(200));
    assert_eq!(settings.accept_poll, Duration::from_millis(20));
    assert_eq!(settings.serial_reopen, Duration::from_millis(250));
    assert_eq!(config.service.stop_wait, Span(Duration::from_secs(10)));
    let pairing = &config.pairing;
    assert_eq!((pairing.pending, pairing.pending_per_source), (32, 4));
    assert_eq!(pairing.ipv6_prefix, 64);
    assert_eq!(pairing.exchange_timeout, Span(Duration::from_secs(5)));
    let client = &config.client;
    assert_eq!(client.reply_timeout, Span(Duration::from_secs(30)));
    assert_eq!(client.transfer(), Some(Duration::from_secs(300)));
    assert_eq!(client.gui_timeout, Span(Duration::from_secs(120)));
    assert_eq!((client.input_chunk, client.read_size), (16 * 1024, 1 << 20));
    assert_eq!(client.input_queue, 4);
    assert_eq!(client.probe_timeout, Span(Duration::from_secs(1)));
    assert_eq!(client.links_wait, Span(Duration::from_secs(5)));
    assert_eq!(
        (
            client.parallel_streams,
            client.parallel_links,
            client.parallel_range
        ),
        (1, 1, 0)
    );
    let waiting = config.waiting.settings();
    assert_eq!(waiting, coqui_peer::WaitingSettings::default());
    assert_eq!(waiting.limits.per_source, 16);
    assert_eq!(waiting.limits.total, 1000);
}

#[test]
fn the_waiting_list_bounds_are_read_and_reach_the_store() {
    let config = parse(
        "[waiting]\nper_source = 2\ntotal = 5\nrerecord = \"5s\"\nrecord_interval = 0\nipv6_prefix = 48\nwhen_full = \"replace-oldest\"\n",
    )
    .unwrap();
    let settings = config.waiting.settings();
    assert_eq!(settings.limits.per_source, 2);
    assert_eq!(settings.limits.total, 5);
    assert_eq!(settings.limits.rerecord, Duration::from_secs(5));
    assert!(settings.record_interval.is_zero());
    assert_eq!(settings.ipv6_prefix, 48);
    assert_eq!(settings.when_full, coqui_peer::WhenFull::ReplaceOldest);
    let dir = tempfile::tempdir().unwrap();
    let store = bohio::peer::Data::new(dir.path())
        .with_waiting(&config.waiting)
        .store()
        .unwrap();
    assert_eq!(store.settings().waiting, settings);
    assert!(parse("[waiting]\nwhen_full = \"sometimes\"\n").is_err());
}

#[test]
fn a_weaker_choice_of_each_new_setting_is_accepted_and_reaches_its_user() {
    let config = parse(
        "[link]\naccept_wait = \"1s\"\naccept_poll = \"1ms\"\nserial_reopen = 0\n\
         [pairing]\npending = 1000\npending_per_source = 500\nipv6_prefix = 128\nexchange_timeout = \"1h\"\n\
         [client]\ntransfer_timeout = 0\ninput_chunk = 1\nread_size = 1\n",
    )
    .unwrap();
    let settings = config.link.settings();
    assert_eq!(settings.accept_wait, Duration::from_secs(1));
    assert!(settings.serial_reopen.is_zero());
    assert_eq!(config.pairing.pending, 1000);
    assert_eq!(
        config.pairing.exchange_timeout,
        Span(Duration::from_secs(3600))
    );
    assert_eq!(config.client.transfer(), None);
}

#[test]
fn the_heartbeat_and_the_dial_settings_have_their_defaults() {
    use bohio::config::DialConfig;
    let config = parse("").unwrap();
    assert_eq!(config.link.heartbeat, Span(Duration::from_secs(25)));
    assert_eq!(config.link.heartbeat_misses, 3);
    let settings = config.link.settings();
    assert_eq!(settings.heartbeat, Duration::from_secs(25));
    assert_eq!(settings.heartbeat_misses, 3);

    let id = peer_id();
    let config = parse(&format!(
        "[peer.\"{id}\"]\nendpoint = \"tcp://host.example:7620\"\n"
    ))
    .unwrap();
    let table = config.peer.table(&id).expect("the table");
    assert_eq!(table.endpoint(), Some("tcp://host.example:7620"));
    assert_eq!(table.dial(), &DialConfig::default());
    let dial = DialConfig::default();
    assert_eq!(dial.timeout, Span(Duration::from_secs(10)));
    assert_eq!(dial.retry_min, Span(Duration::from_secs(1)));
    assert_eq!(dial.retry_max, Span(Duration::from_secs(60)));
    assert!((dial.retry_factor - 2.0).abs() < f64::EPSILON);
    assert!((dial.retry_jitter - 0.1).abs() < f64::EPSILON);
    assert!(dial.ca.is_empty() && dial.cert_hash.is_empty() && dial.fallback.is_none());
    assert_eq!(config.peer.dials().count(), 1);
    // A peer without an endpoint is accepted and not dialed.
    let other = peer_id();
    let config = parse(&format!("[peer.\"{other}\"]\nallow = [\"info\"]\n")).unwrap();
    assert_eq!(config.peer.dials().count(), 0);
}

#[test]
fn a_heartbeat_of_zero_is_off_and_every_dial_setting_is_read() {
    let id = peer_id();
    let config = parse(&format!(
        "[link]\nheartbeat = 0\nheartbeat_misses = 1\n\
         [peer.\"{id}\"]\nendpoint = \"webtransport://host.example:7622/bohio\"\n\
         [peer.\"{id}\".dial]\nca = [\"ca.pem\"]\nfallback = \"wss://host.example:7622/bohio\"\n\
         timeout = \"3s\"\nretry_min = \"200ms\"\nretry_max = \"5s\"\nretry_factor = 1\nretry_jitter = 0\n"
    ))
    .unwrap();
    assert!(config.link.heartbeat.0.is_zero());
    assert!(config.link.settings().limits().heartbeat.is_none());
    let dial = config.peer.table(&id).unwrap().dial();
    assert_eq!(dial.ca, [PathBuf::from("ca.pem")]);
    assert_eq!(dial.timeout, Span(Duration::from_secs(3)));
    assert_eq!(dial.retry_min, Span(Duration::from_millis(200)));
    assert!((dial.retry_factor - 1.0).abs() < f64::EPSILON);
    assert!(dial.retry_jitter.abs() < f64::EPSILON);
}

#[test]
fn a_dial_setting_that_means_nothing_is_refused_naming_its_key() {
    let id = peer_id();
    let table = |dial: &str, endpoint: &str| {
        format!("[peer.\"{id}\"]\nendpoint = \"{endpoint}\"\n[peer.\"{id}\".dial]\n{dial}\n")
    };
    let tcp = "tcp://host.example:7620";
    for (text, key) in [
        (table("timeout = 0", tcp), "timeout"),
        (table("retry_min = 0", tcp), "retry_min"),
        (
            table("retry_min = \"5s\"\nretry_max = \"1s\"", tcp),
            "retry_max",
        ),
        (table("retry_factor = 0.5", tcp), "retry_factor"),
        (table("retry_jitter = 1.5", tcp), "retry_jitter"),
        (table("retry_jitter = -0.1", tcp), "retry_jitter"),
        (table("cert_hash = [\"not a hash\"]", tcp), "cert_hash"),
    ] {
        match parse(&text) {
            Err(ConfigError::Invalid { key: found, .. }) => {
                assert_eq!(found, format!("peer.{id}.dial.{key}"), "{text}");
            }
            other => panic!("{text}: expected Invalid, got {other:?}"),
        }
    }
    for (text, key) in [
        (table("", "nowhere"), "endpoint"),
        (
            table(
                "fallback = \"tcp://127.0.0.1:7620\"",
                "webtransport://host.example:7622/b",
            ),
            "endpoint",
        ),
        (
            "[link]\nheartbeat_misses = 0\n".to_owned(),
            "link.heartbeat_misses",
        ),
    ] {
        match parse(&text) {
            Err(ConfigError::Invalid { key: found, .. }) => {
                let want = if key.starts_with("link.") {
                    key.to_owned()
                } else {
                    format!("peer.{id}.{key}")
                };
                assert_eq!(found, want, "{text}");
            }
            other => panic!("{text}: expected Invalid, got {other:?}"),
        }
    }
    // A key this version does not know in `[dial]` is a parse error, like everywhere else.
    assert!(matches!(
        parse(&table("retries = 3", tcp)),
        Err(ConfigError::Parse { .. })
    ));
}

#[test]
fn the_window_and_open_limits_have_their_defaults_and_reach_the_link() {
    let config = parse("").unwrap();
    let link = &config.link;
    assert_eq!((link.max_window, link.bulk_window), (None, None));
    assert!(!link.auto_window);
    assert_eq!(link.max_metadata, 4096);
    assert_eq!(link.max_pending_opens, 8);
    assert_eq!(link.max_opens_per_minute, 600);
    assert_eq!(
        (link.tcp_send_buffer, link.tcp_receive_buffer),
        (None, None)
    );

    let settings = link.settings();
    assert_eq!(settings.max_window, settings.window);
    assert_eq!(settings.bulk_window, settings.window);
    assert!(!settings.auto_window);
    let limits = settings.limits();
    assert_eq!(limits.window, 1024 * 1024);
    assert_eq!(limits.max_window, limits.window);
    assert!(!limits.auto_window);
    assert_eq!(limits.max_metadata, 4096);
    assert_eq!(limits.max_pending_opens, 8);
    assert_eq!(limits.max_opens_per_minute, 600);
    assert_eq!(limits.open_wait, Duration::from_millis(250));
}

#[test]
fn every_window_and_open_limit_is_read_and_reaches_the_limits() {
    let config = parse(
        "[link]\nwindow = 65536\nmax_window = 16777216\nbulk_window = 8388608\nauto_window = true\n\
         max_metadata = 65536\nmax_pending_opens = 32\nmax_opens_per_minute = 6000\nopen_wait = \"2s\"\n\
         tcp_send_buffer = 4194304\ntcp_receive_buffer = 2097152\n",
    )
    .unwrap();
    let settings = config.link.settings();
    assert_eq!(settings.bulk_window, 8 * 1024 * 1024);
    assert_eq!(settings.tcp_send_buffer, Some(4 * 1024 * 1024));
    assert_eq!(settings.tcp_receive_buffer, Some(2 * 1024 * 1024));
    let limits = settings.limits();
    assert_eq!(limits.window, 65536);
    assert_eq!(limits.max_window, 16 * 1024 * 1024);
    assert!(limits.auto_window);
    assert_eq!(limits.max_metadata, 65536);
    assert_eq!(limits.max_pending_opens, 32);
    assert_eq!(limits.max_opens_per_minute, 6000);
    assert_eq!(limits.open_wait, Duration::from_secs(2));
}

#[test]
fn a_bulk_window_alone_raises_the_most_a_window_may_grow_to_and_a_smaller_one_is_allowed() {
    let settings = parse("[link]\nbulk_window = 8388608\n")
        .unwrap()
        .link
        .settings();
    assert_eq!(settings.max_window, 8 * 1024 * 1024);
    // A window is only widened, so a bulk window below `window` changes nothing and is not an error.
    let settings = parse("[link]\nbulk_window = 4096\n")
        .unwrap()
        .link
        .settings();
    assert_eq!(settings.bulk_window, 4096);
    assert_eq!(settings.max_window, settings.window);
}

#[test]
fn a_window_limit_that_means_nothing_is_refused_naming_its_key() {
    for (text, key) in [
        ("[link]\nmax_window = 0\n", "link.max_window"),
        ("[link]\nbulk_window = 0\n", "link.bulk_window"),
        ("[link]\nmax_metadata = 0\n", "link.max_metadata"),
        ("[link]\nmax_pending_opens = 0\n", "link.max_pending_opens"),
        (
            "[link]\nmax_opens_per_minute = 0\n",
            "link.max_opens_per_minute",
        ),
        ("[link]\ntcp_send_buffer = 0\n", "link.tcp_send_buffer"),
        (
            "[link]\ntcp_receive_buffer = 0\n",
            "link.tcp_receive_buffer",
        ),
        // The window and the bulk window are at most the most a window may grow to.
        ("[link]\nmax_window = 524288\n", "link.window"),
        (
            "[link]\nmax_window = 2097152\nbulk_window = 4194304\n",
            "link.bulk_window",
        ),
    ] {
        match parse(text) {
            Err(ConfigError::Invalid { key: found, .. }) => assert_eq!(found, key, "{text}"),
            other => panic!("{text}: expected Invalid, got {other:?}"),
        }
    }
}

#[test]
fn a_weaker_compression_choice_is_a_choice_not_an_error() {
    // Nothing allowed, no trial, no expansion bound, a window at the floor: each is meaningful, with a cost the
    // documentation names, so each loads.
    let config = parse(
        "[compression]\nallowed = []\nsample_bytes = 0\nmax_expansion = 0\nwindow_log_max = 10\n\
         skip_extensions = []\nexclude = []\nsample_ratio = 5.0\n",
    )
    .unwrap();
    assert!(config.compression.allowed.is_empty());
    assert_eq!(config.compression.max_expansion, 0);
    // A host that allows nothing reports no default and no algorithm.
    let info = bohio::info::collect(&config.node(), std::ffi::OsStr::new(""));
    assert!(info.transfer.compression.is_empty() && info.transfer.compression_default.is_none());
}

#[cfg(not(feature = "zstd"))]
#[test]
fn without_zstd_it_cannot_be_named_and_workers_need_the_feature() {
    assert!(matches!(
        parse("[compression]\nallowed = [\"zstd\"]\n"),
        Err(ConfigError::Invalid { .. })
    ));
}

#[cfg(not(feature = "zstdmt"))]
#[test]
fn zstd_worker_threads_need_a_build_that_has_them() {
    match parse("[compression]\nworkers = 2\n") {
        Err(ConfigError::Invalid { key, .. }) => assert_eq!(key, "compression.workers"),
        other => panic!("expected Invalid, got {other:?}"),
    }
}

#[test]
fn the_mtu_settings_default_to_quinns_and_are_read_for_a_listener_and_a_dialer() {
    let defaults = bohio::config::DialConfig::default();
    let mtu = defaults.web_mtu().unwrap();
    assert_eq!(
        (mtu.initial, mtu.min, mtu.discovery, mtu.upper_bound),
        (1200, 1200, true, 1452)
    );
    let config = parse(
        "[[listen]]\nendpoint = \"webtransport://127.0.0.1:7622\"\n\
         mtu_initial = 1280\nmtu_min = 1250\nmtu_discovery = false\nmtu_upper_bound = 1400\n",
    )
    .unwrap();
    let mtu = config.listen[0].web_mtu().unwrap();
    assert_eq!(
        (mtu.initial, mtu.min, mtu.discovery, mtu.upper_bound),
        (1280, 1250, false, 1400)
    );
    assert_eq!(
        parse("[[listen]]\nendpoint = \"webtransport://127.0.0.1:7622\"\n")
            .unwrap()
            .listen[0]
            .web_mtu()
            .unwrap(),
        bohio_client::dial::Mtu::default()
    );
    let id = peer_id();
    let config = parse(&format!(
        "[peer.\"{id}\"]\nendpoint = \"webtransport://host.example:7622\"\n\
         [peer.\"{id}\".dial]\nmtu_initial = 1300\nmtu_discovery = false\nmtu_upper_bound = 1300\n"
    ))
    .unwrap();
    let mtu = config.peer.table(&id).unwrap().dial().web_mtu().unwrap();
    assert_eq!(
        (mtu.initial, mtu.min, mtu.discovery, mtu.upper_bound),
        (1300, 1200, false, 1300)
    );
}

#[test]
fn an_mtu_below_the_least_or_for_another_endpoint_is_refused_naming_it() {
    let id = peer_id();
    let table = |dial: &str, endpoint: &str| {
        format!("[peer.\"{id}\"]\nendpoint = \"{endpoint}\"\n[peer.\"{id}\".dial]\n{dial}\n")
    };
    let web = "webtransport://host.example:7622";
    for (text, key, value) in [
        (table("mtu_initial = 1100", web), "mtu_initial", "1100"),
        (table("mtu_min = 900", web), "mtu_min", "900"),
    ] {
        match parse(&text) {
            Err(ConfigError::Invalid {
                key: found,
                message: reason,
                ..
            }) => {
                assert_eq!(found, format!("peer.{id}.dial.{key}"), "{text}");
                assert!(
                    reason.contains(value) && reason.contains("1200"),
                    "{reason}"
                );
            }
            other => panic!("{text}: {other:?}"),
        }
    }
    match parse(&table("mtu_upper_bound = 1300", "tcp://host.example:7620")) {
        Err(ConfigError::Invalid { key, .. }) => assert_eq!(key, format!("peer.{id}.dial")),
        other => panic!("{other:?}"),
    }
}

const HOSTS: &str = r#"
[remote.web]
endpoint = "tcp://10.0.0.5:7620"
server = "sha256:2222222222222222222222222222222222222222222222222222222222222222"
jump = ["gateway", "tcp://inner.example:7620"]

[remote.gateway]
endpoint = "tcp://gateway.example:7620"
server = "sha256:3333333333333333333333333333333333333333333333333333333333333333"
jump = ["edge"]

[remote.edge]
endpoint = "tcp://edge.example:7620"
"#;

#[test]
fn named_hosts_are_read_and_a_named_jump_brings_its_own_jumps_and_server() {
    let config = parse(HOSTS).expect("the hosts are valid");
    assert_eq!(config.remote.len(), 3);
    assert_eq!(
        config.remote.names().collect::<Vec<_>>(),
        ["edge", "gateway", "web"]
    );
    let web = config.remote.get("web").unwrap();
    assert_eq!(web.endpoint, "tcp://10.0.0.5:7620");
    // The hops in order: the gateway's own jump first, then the gateway, then an endpoint named as it is.
    let hops = config.remote.hops("web").unwrap();
    let endpoints: Vec<&str> = hops.iter().map(|hop| hop.endpoint.as_str()).collect();
    assert_eq!(
        endpoints,
        [
            "tcp://edge.example:7620",
            "tcp://gateway.example:7620",
            "tcp://inner.example:7620"
        ]
    );
    assert_eq!(hops[0].server, None);
    assert!(hops[1].server.as_deref().unwrap().ends_with("3333"));
    assert_eq!(hops[2].server, None);
    assert!(config.remote.hops("nowhere").is_err());
}

fn invalid(text: &str) -> (String, String) {
    match parse(text) {
        Err(ConfigError::Invalid { key, message, .. }) => (key, message),
        other => panic!("expected an invalid configuration, got {other:?}"),
    }
}

#[test]
fn a_jump_that_names_no_host_a_cycle_and_a_host_without_an_endpoint_are_refused_naming_the_key() {
    let (key, message) = invalid("[remote.web]\nendpoint = \"tcp://a:1\"\njump = [\"missing\"]\n");
    assert_eq!(key, "remote.web.jump");
    assert!(message.contains("missing"), "{message}");

    // A cycle through two hosts, and a host that is its own jump.
    let (key, message) = invalid(
        "[remote.a]\nendpoint = \"tcp://a:1\"\njump = [\"b\"]\n[remote.b]\nendpoint = \"tcp://b:1\"\njump = [\"a\"]\n",
    );
    assert_eq!(key, "remote.a.jump");
    assert!(message.contains("a -> b -> a"), "{message}");
    let (key, _) = invalid("[remote.me]\nendpoint = \"tcp://a:1\"\njump = [\"me\"]\n");
    assert_eq!(key, "remote.me.jump");

    let (key, _) = invalid("[remote.web]\nserver = \"sha256:ab\"\n");
    assert_eq!(key, "remote.web.endpoint");
    // A key this version does not know is named, as everywhere else.
    assert!(matches!(
        parse("[remote.web]\nendpoint = \"tcp://a:1\"\nport = 3\n"),
        Err(ConfigError::Parse { .. })
    ));
    // A chain that does not loop is fine.
    assert!(parse(HOSTS).is_ok());
}

#[test]
fn the_agentless_console_node_example_is_accepted() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/console_node.cfg");
    let text = std::fs::read_to_string(path).expect("the example is there");
    let config = Config::from_toml(&text, "console_node.cfg").expect("it is accepted");
    assert_eq!(config.access.terminals(), ["console"]);
    assert!(config.access.desktops().is_empty());
    assert_eq!(config.access.fs, Some(Vec::new()));
    assert!(config.source.contains_key("console"));
}

// Profiles: named defaults laid under the files.

fn profile_example(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/profiles")
        .join(format!("{name}.cfg"))
}

const MIB: u32 = 1024 * 1024;

#[test]
fn every_profile_loads_with_the_values_it_names() {
    use bohio_client::profile::Profile;
    let load = |name: &str| parse(&format!("[profile]\nname = \"{name}\"\n")).unwrap();

    let default = load("default");
    assert_eq!(default, Config::default());

    let lan = load("lan");
    assert_eq!(lan.filesystem.chunk_size, 256 * 1024);
    assert_eq!(lan.link.bulk_window, Some(4 * MIB));
    assert_eq!(lan.link.max_window, Some(8 * MIB));
    assert_eq!(
        lan.compression.bundle_context,
        bohio::config::BundleContext::Shared
    );
    assert_eq!(
        default.compression.bundle_context,
        bohio::config::BundleContext::PerItem
    );

    let weak = load("weak-host");
    assert_eq!(weak.hash.max_streams, 1);
    assert_eq!(weak.walk.max_streams, 1);
    assert_eq!(weak.filesystem.max_transfers, 4);
    assert_eq!(weak.link.max_window, Some(MIB));
    assert!(!weak.link.auto_window);
    assert_eq!(weak.delta.max_signature_bytes, 8 * 1024 * 1024);
    assert_eq!(weak.compression.max_level, 3);
    assert_eq!(weak.compression.window_log_max, 20);
    assert_eq!(weak.compression.default.algorithm.name(), "lz4");
    assert_eq!(
        weak.compression
            .allowed
            .iter()
            .map(|codec| codec.name().to_owned())
            .collect::<Vec<_>>(),
        ["lz4", "zstd"]
    );

    let slow = load("slow-link");
    assert_eq!(slow.compression.default.algorithm.name(), "zstd");
    assert_eq!(slow.compression.default.level, Some(19));
    assert_eq!(slow.compression.window_log_max, 27);
    assert_eq!(slow.compression.prepare_max_bytes, 4 << 30);
    assert_eq!(slow.link.bulk_window, Some(4 * MIB));
    assert_eq!(slow.link.max_window, Some(16 * MIB));
    assert!(slow.link.auto_window);
    assert_eq!(slow.link.max_opens_per_minute, 6000);
    assert_eq!(slow.filesystem.resume_ttl, Span(Duration::from_secs(3600)));

    let fat = load("fat-link");
    assert_eq!(fat.link.bulk_window, Some(8 * MIB));
    assert_eq!(fat.link.max_window, None);
    let mut rest = fat;
    rest.link.bulk_window = None;
    rest.profile = Config::default().profile;
    assert_eq!(
        rest,
        Config::default(),
        "fat-link sets bulk_window and nothing else"
    );

    // A node an embedding application starts takes the same table.
    for profile in Profile::ALL {
        let from_text = parse(&format!("[profile]\nname = \"{}\"\n", profile.name())).unwrap();
        assert_eq!(
            bohio::config::NodeConfig::with_profile(profile),
            from_text.node(),
            "{}",
            profile.name()
        );
    }
}

#[test]
fn a_key_the_file_sets_beats_the_profile_and_the_rest_stays() {
    let config = parse(
        "[profile]\nname = \"slow-link\"\n[link]\nbulk_window = 1048576\n[compression]\nwindow_log_max = 23\n",
    )
    .unwrap();
    assert_eq!(config.link.bulk_window, Some(MIB));
    assert_eq!(config.compression.window_log_max, 23);
    // What the file did not set is the profile's.
    assert_eq!(config.link.max_window, Some(16 * MIB));
    assert_eq!(config.compression.default.level, Some(19));
}

#[test]
fn the_highest_layer_names_the_profile_and_a_lower_layers_key_still_beats_it() {
    use bohio::config::Origin;
    let dir = tempfile::tempdir().unwrap();
    let files = layers(
        dir.path(),
        &[
            (
                "system.cfg",
                "[profile]\nname = \"lan\"\n[link]\nbulk_window = 2097152\n",
            ),
            ("user.cfg", "[profile]\nname = \"fat-link\"\n"),
        ],
    );
    let layered = Layers::load(&files).unwrap();
    assert_eq!(layered.profile.name(), "fat-link");
    // The system layer set it: it wins over the profile, though the user layer chose the profile.
    assert_eq!(layered.config.link.bulk_window, Some(2 * MIB));
    // `lan` is not the profile in force: its keys do not apply.
    assert_eq!(layered.config.filesystem.chunk_size, 64 * 1024);
    assert!(
        matches!(layered.origin("link.bulk_window"), Origin::File(label) if label.ends_with("system.cfg"))
    );
    assert_eq!(layered.origin("filesystem.chunk_size"), Origin::Default);
}

#[test]
fn origins_say_file_profile_or_default() {
    use bohio::config::Origin;
    let layered = Layers::parse(
        "[profile]\nname = \"slow-link\"\n[link]\nbulk_window = 1048576\n",
        "bohio.cfg",
    )
    .unwrap();
    assert_eq!(
        layered.origin("link.bulk_window"),
        Origin::File("bohio.cfg".to_owned())
    );
    assert_eq!(
        layered.origin("link.max_window").to_string(),
        "profile slow-link"
    );
    assert_eq!(layered.origin("link.window"), Origin::Default);
    assert_eq!(layered.origin("link.window").to_string(), "default");
    assert!(layered.sets("link.bulk_window"));
    assert!(!layered.sets("link.max_window"));
}

#[test]
fn an_unknown_profile_is_refused_naming_its_key_and_the_names() {
    match parse("[profile]\nname = \"turbo\"\n").unwrap_err() {
        ConfigError::Invalid { path, key, message } => {
            assert_eq!(key, "profile.name");
            assert_eq!(path, "bohio.cfg");
            assert!(
                message.contains("turbo") && message.contains("slow-link"),
                "{message}"
            );
        }
        other => panic!("{other:?}"),
    }
    // In a layer, the layer that names it is the one reported.
    let dir = tempfile::tempdir().unwrap();
    let files = layers(
        dir.path(),
        &[
            ("system.cfg", "[profile]\nname = \"lan\"\n"),
            ("user.cfg", "[profile]\nname = \"fast\"\n"),
        ],
    );
    match Layers::load(&files).unwrap_err() {
        ConfigError::Invalid { path, key, .. } => {
            assert!(path.ends_with("user.cfg"), "{path}");
            assert_eq!(key, "profile.name");
        }
        other => panic!("{other:?}"),
    }
    // A client profile is checked the same way.
    match parse("[client]\nprofile = \"turbo\"\n").unwrap_err() {
        ConfigError::Invalid { key, .. } => assert_eq!(key, "client.profile"),
        other => panic!("{other:?}"),
    }
    assert!(parse("[client]\nprofile = \"slow-link\"\n").is_ok());
}

#[test]
fn an_error_in_a_value_only_the_profile_supplied_names_the_profile() {
    // slow-link's default compression is zstd; the file allows only lz4 and says nothing of the default.
    match parse("[profile]\nname = \"slow-link\"\n[compression]\nallowed = [\"lz4\"]\n")
        .unwrap_err()
    {
        ConfigError::Invalid { path, key, .. } => {
            assert_eq!(key, "compression.default");
            assert_eq!(path, "profile slow-link");
        }
        other => panic!("{other:?}"),
    }
    // The same conflict with a key the file does set names the file.
    match parse("[profile]\nname = \"weak-host\"\n[link]\nwindow = 4194304\n").unwrap_err() {
        ConfigError::Invalid { path, key, .. } => {
            assert_eq!(key, "link.window");
            assert_eq!(path, "bohio.cfg");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_profile_sets_no_peer_and_every_key_it_sets_is_one_config_check_prints() {
    use bohio_client::profile::Profile;
    let printed: Vec<&str> = Config::default()
        .transfer_keys()
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    for profile in Profile::ALL {
        let text = bohio::profile::table(profile);
        let table: toml::Table = toml::from_str(text).unwrap();
        assert!(!table.contains_key("peer"), "{}", profile.name());
        for (section, keys) in &table {
            for key in keys.as_table().unwrap().keys() {
                let path = format!("{section}.{key}");
                assert!(
                    printed.contains(&path.as_str()),
                    "{}: {path} is not printed by config check",
                    profile.name()
                );
            }
        }
    }
}

#[test]
fn the_profile_example_files_load_under_their_own_profile() {
    for name in ["lan", "weak-host", "slow-link", "fat-link"] {
        let layered = Layers::load(&[profile_example(name)]).unwrap();
        assert_eq!(layered.profile.name(), name);
    }
}

#[test]
fn the_client_and_mcp_bounds_have_documented_defaults_the_file_can_change() {
    let defaults = parse("").unwrap();
    assert_eq!(defaults.client.power_wait.0, Duration::from_secs(90));
    assert_eq!(defaults.client.kill_after.0, Duration::from_secs(2));
    assert_eq!(defaults.client.sync_bundle_max, 1_048_576);
    assert_eq!(defaults.client.sync_bundle_items, 1000);
    assert_eq!(defaults.mcp.max_calls, 8);
    assert_eq!(defaults.mcp.gui_max_image_bytes, 1_048_576);
    let set = parse(
        "[client]\npower_wait = \"5m\"\nkill_after = \"500ms\"\nsync_bundle_max = 10\nsync_bundle_items = 3\n\
         [mcp]\nmax_calls = 2\ngui_max_image_bytes = 5\n",
    )
    .unwrap();
    assert_eq!(set.client.power_wait.0, Duration::from_secs(300));
    assert_eq!(set.client.kill_after.0, Duration::from_millis(500));
    assert_eq!(
        (set.client.sync_bundle_max, set.client.sync_bundle_items),
        (10, 3)
    );
    assert_eq!((set.mcp.max_calls, set.mcp.gui_max_image_bytes), (2, 5));
}
