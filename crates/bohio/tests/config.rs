//! `bohio.cfg`: defaults, refusals, the layers and the peers they hold, and the bounds.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bohio::config::{Config, ConfigError, Environment, Layers, Span};
use bohio::policy::Permission;
use coqui_link::auth::{Identity, Method};

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
        ("[shell]\nmax_sessions = 0\n", "shell.max_sessions"),
        ("[shell]\nchunk_size = 0\n", "shell.chunk_size"),
        ("[shell]\npoll_interval = 0\n", "shell.poll_interval"),
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

#[test]
fn a_governing_key_set_above_the_lowest_layer_is_refused_naming_the_layer() {
    let dir = tempfile::tempdir().unwrap();
    for key in ["check_access = false", "user_layer = false"] {
        let files = layers(
            dir.path(),
            &[
                ("system.cfg", "[service]\nname = \"s\"\n"),
                ("user.cfg", &format!("[service]\n{key}\n")),
            ],
        );
        match Layers::load(&files) {
            Err(ConfigError::Invalid {
                path, key: found, ..
            }) => {
                assert!(path.ends_with("user.cfg"), "{path}");
                assert_eq!(found, format!("service.{}", key.split(' ').next().unwrap()));
            }
            other => panic!("{key}: expected Invalid, got {other:?}"),
        }
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
