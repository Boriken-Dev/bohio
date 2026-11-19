//! `bohio.cfg` and `peers.cfg`: defaults, refusals, the search order, and the bounds.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bohio::config::{Config, ConfigError, Environment, Span};
use bohio::policy::{Permission, Policies, PolicyError};

fn example() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/bohio.cfg")
}

fn parse(text: &str) -> Result<Config, ConfigError> {
    Config::from_toml(text, "bohio.cfg")
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
        app_data: Some("A".into()),
        local_app_data: Some("L".into()),
        program_data: Some("P".into()),
        xdg_config_home: Some("X".into()),
        xdg_data_home: Some("D".into()),
        home: Some("H".into()),
        ..Environment::default()
    }
}

#[test]
fn the_search_order_is_the_variable_then_the_user_then_the_system() {
    let mut unix = environment(false);
    unix.config = Some("/named.cfg".into());
    let found = unix.config_candidates();
    assert_eq!(found[0], Path::new("/named.cfg"));
    assert_eq!(found[1], Path::new("X").join("bohio").join("bohio.cfg"));
    assert_eq!(found[2], Path::new("/etc/bohio/bohio.cfg"));

    let mut no_xdg = environment(false);
    no_xdg.xdg_config_home = None;
    assert_eq!(
        no_xdg.config_candidates()[0],
        Path::new("H")
            .join(".config")
            .join("bohio")
            .join("bohio.cfg")
    );

    let windows = environment(true).config_candidates();
    assert_eq!(windows[0], Path::new("A").join("bohio").join("bohio.cfg"));
    assert_eq!(windows[1], Path::new("P").join("bohio").join("bohio.cfg"));
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
fn locate_uses_an_explicit_file_then_the_environment_then_the_defaults() {
    let directory = std::env::temp_dir().join(format!("bohio-config-test-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let named = directory.join("named.cfg");
    std::fs::write(&named, "[service]\nname = \"named\"\n").unwrap();
    let user = directory.join("bohio").join("bohio.cfg");
    std::fs::create_dir_all(user.parent().unwrap()).unwrap();
    std::fs::write(&user, "[service]\nname = \"user\"\n").unwrap();

    // The only file read is the explicit one.
    let none = Environment::default();
    let (config, from) = Config::locate(Some(&named), &none).unwrap();
    assert_eq!(
        (config.service.name.as_str(), from.as_deref()),
        ("named", Some(named.as_path()))
    );

    // BOHIO_CONFIG beats the user's file.
    let by_variable = Environment {
        config: Some(named.clone()),
        xdg_config_home: Some(directory.clone()),
        ..Environment::default()
    };
    assert_eq!(
        Config::locate(None, &by_variable).unwrap().0.service.name,
        "named"
    );

    // Without it, the user's file.
    let by_user = Environment {
        xdg_config_home: Some(directory.clone()),
        ..Environment::default()
    };
    assert_eq!(
        Config::locate(None, &by_user).unwrap().0.service.name,
        "user"
    );

    // A named file that is missing is an error; no file at all is the defaults.
    let missing = Environment {
        config: Some(directory.join("missing.cfg")),
        ..Environment::default()
    };
    assert!(matches!(
        Config::locate(None, &missing),
        Err(ConfigError::Read { .. })
    ));
    let (config, from) = Config::locate(None, &Environment::default()).unwrap();
    assert!(from.is_none() || from.as_deref() == Some(Path::new("/etc/bohio/bohio.cfg")));
    let _ = config;

    std::fs::remove_dir_all(&directory).unwrap();
}

#[test]
fn peers_cfg_reads_each_peers_grant() {
    let policies = Policies::from_toml(
        r#"
[peer.laptop]
allow = ["info", "fs.read", "exec"]
fs_roots = ["/srv/data"]

[peer.laptop.exec]
programs = ["git", "make"]
env = ["LANG"]

[peer.guest]
allow = ["info"]
"#,
        "peers.cfg",
    )
    .unwrap();
    let laptop = policies.grant_for("laptop");
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
    let guest = policies.grant_for("guest");
    assert!(guest.allows(Permission::Info) && !guest.allows(Permission::Exec));
    assert!(guest.permits_program("anything") && guest.permits_shell_script());
    assert!(
        guest.permits_env(&lang),
        "without a program list any variable may be set"
    );
    assert_eq!(policies.peers().collect::<Vec<_>>(), ["guest", "laptop"]);
}

#[test]
fn an_unknown_peer_and_an_approved_peer_with_no_allow_get_nothing() {
    let policies = Policies::from_toml("[peer.quiet]\n", "peers.cfg").unwrap();
    for id in ["quiet", "stranger"] {
        let grant = policies.grant_for(id);
        assert_eq!(grant.permissions().count(), 0, "{id}");
        assert!(grant.fs_roots().is_empty());
    }
}

#[test]
fn peers_cfg_refuses_an_unknown_permission_naming_it() {
    for permission in ["root", "fs.rw", "Exec", ""] {
        let text = format!("[peer.x]\nallow = [\"{permission}\"]\n");
        let error = Policies::from_toml(&text, "peers.cfg").unwrap_err();
        let PolicyError::Parse { line, .. } = &error else {
            panic!("expected a parse error for {permission:?}, got {error:?}");
        };
        assert_eq!(*line, 2, "{error}");
        assert!(error.to_string().contains("unknown variant"), "{error}");
    }
}

#[test]
fn peers_cfg_refuses_unknown_keys() {
    assert!(Policies::from_toml("[peer.x]\nallow = []\nfs_root = []\n", "p").is_err());
    assert!(Policies::from_toml("[peer.x.exec]\nprogram = []\n", "p").is_err());
    assert!(Policies::from_toml("[peers]\n", "p").is_err());
}

#[test]
fn a_missing_peers_file_is_an_empty_policy() {
    let path = std::env::temp_dir().join("bohio-no-such-peers.cfg");
    assert_eq!(Policies::load(&path).unwrap(), Policies::default());
}

#[test]
fn every_permission_name_round_trips_through_the_file() {
    for permission in [
        Permission::Info,
        Permission::FsRead,
        Permission::FsWrite,
        Permission::Exec,
        Permission::Shell,
        Permission::Admin,
    ] {
        let text = format!("[peer.p]\nallow = [\"{}\"]\n", permission.name());
        assert!(
            Policies::from_toml(&text, "p")
                .unwrap()
                .grant_for("p")
                .allows(permission)
        );
    }
}
