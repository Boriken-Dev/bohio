//! The built `bohio` binary, run with real arguments.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use clap::{CommandFactory, Parser};

fn bohio(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bohio"))
        .args(args)
        // The host's own configuration and PATH must not leak into a test.
        .env_remove("BOHIO_CONFIG")
        .env_remove("BOHIO_LOG")
        .output()
        .expect("the binary runs")
}

fn example() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/bohio.cfg")
        .to_string_lossy()
        .into_owned()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn config_check_accepts_the_example_configuration() {
    let output = bohio(&["config", "check", "--config", &example()]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let out = text(&output.stdout);
    assert!(out.starts_with("ok: "), "{out}");
    assert!(out.contains("listen: 1"), "{out}");
}

#[test]
fn config_check_rejects_an_unknown_key_naming_it_and_exits_1() {
    let directory = scratch("check-unknown");
    let file = directory.join("bad.cfg");
    std::fs::write(&file, "[execution]\nmax_procesess = 4\n").unwrap();
    let output = bohio(&["config", "check", "--config", &file.to_string_lossy()]);
    assert_eq!(output.status.code(), Some(1));
    let err = text(&output.stderr);
    assert!(err.starts_with("bohio: "), "{err}");
    assert!(
        err.contains("max_procesess") && err.contains(":2:"),
        "{err}"
    );
    assert_eq!(err.lines().count(), 1, "an error is one line: {err}");
    assert!(output.stdout.is_empty());
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn config_check_of_a_missing_file_fails_and_a_missing_default_file_does_not() {
    let missing = bohio(&["config", "check", "--config", "definitely-not-here.cfg"]);
    assert_eq!(missing.status.code(), Some(1));
    let defaults = bohio(&["config", "check"]);
    assert_eq!(
        defaults.status.code(),
        Some(0),
        "{}",
        text(&defaults.stderr)
    );
}

#[test]
fn config_check_json_is_one_document() {
    let output = bohio(&[
        "config",
        "check",
        "--config",
        &example(),
        "--format",
        "json",
    ]);
    assert_eq!(output.status.code(), Some(0));
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one JSON document");
    assert_eq!(report["ok"], true);
    assert_eq!(report["listen"][0], "tcp://127.0.0.1:7620");
}

#[test]
fn info_prints_the_hosts_capabilities() {
    let output = bohio(&["info", "--config", &example()]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    let out = text(&output.stdout);
    assert!(
        out.contains(&format!(
            "os: {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )),
        "{out}"
    );
    assert!(out.contains("protocol: 1"), "{out}");
    assert!(out.contains("exec: enabled"), "{out}");
    assert!(
        out.contains("limits: max_streams=64 max_processes=16 max_sessions=8"),
        "{out}"
    );
}

#[test]
fn info_json_decodes_as_the_protocols_info() {
    let output = bohio(&["info", "--config", &example(), "--format", "json"]);
    assert_eq!(output.status.code(), Some(0));
    let info: bohio_proto::info::Info = serde_json::from_slice(&output.stdout).expect("Info");
    assert_eq!(info.protocol, bohio_proto::PROTOCOL_VERSION);
    assert_eq!(info.os, std::env::consts::OS);
    assert_eq!(info.arch, std::env::consts::ARCH);
    assert!(info.filesystem.enabled && info.exec.enabled && info.shell.enabled);
    assert_eq!(info.filesystem.unix_modes, cfg!(unix));
    assert_eq!(info.filesystem.windows_attributes, cfg!(windows));
    assert_eq!(
        info.filesystem.separator,
        std::path::MAIN_SEPARATOR.to_string()
    );
    assert_eq!(info.limits.max_processes, Some(16));
    assert!(
        !info.shells.is_empty(),
        "a host this test runs on has a shell on PATH"
    );
}

#[test]
fn info_reports_a_disabled_capability_as_disabled() {
    let directory = scratch("info-disabled");
    let file = directory.join("off.cfg");
    std::fs::write(
        &file,
        "[execution]\nenabled = false\n[shell]\nenabled = false\n",
    )
    .unwrap();
    let output = bohio(&[
        "info",
        "--config",
        &file.to_string_lossy(),
        "--format",
        "json",
    ]);
    let info: bohio_proto::info::Info = serde_json::from_slice(&output.stdout).unwrap();
    assert!(!info.exec.enabled && !info.shell.enabled && info.filesystem.enabled);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_wrong_invocation_exits_2() {
    assert_eq!(bohio(&[]).status.code(), Some(2));
    assert_eq!(bohio(&["nonsense"]).status.code(), Some(2));
    assert_eq!(
        bohio(&["config", "check", "--format", "xml"]).status.code(),
        Some(2)
    );
    assert_eq!(bohio(&["info", "--config"]).status.code(), Some(2));
}

#[test]
fn help_lists_the_environment_variables_the_program_reads() {
    let output = bohio(&["--help"]);
    assert_eq!(output.status.code(), Some(0));
    let help = text(&output.stdout);
    for name in ["BOHIO_CONFIG", "BOHIO_LOG", "NO_COLOR", "PATH"] {
        assert!(help.contains(name), "--help names {name}");
    }
}

#[test]
fn the_argument_definition_is_consistent() {
    bohio::cli::Cli::command().debug_assert();
    assert!(bohio::cli::Cli::try_parse_from(["bohio", "info"]).is_ok());
}

/// Makes `path` unwritable for this account (or writable again): the read-only attribute on Windows, the
/// owner's read-only and read-write modes elsewhere, so a file never becomes writable by others.
fn read_only(path: &Path, on: bool) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = if on { 0o400 } else { 0o600 };
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    #[cfg(not(unix))]
    {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_readonly(on);
        std::fs::set_permissions(path, permissions).unwrap();
    }
}

fn scratch(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("bohio-cli-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    directory
}

#[test]
fn service_install_refuses_a_bad_name_before_it_touches_the_service_manager() {
    let output = bohio(&["service", "install", "--name", "a/b"]);
    assert_eq!(output.status.code(), Some(1));
    let err = text(&output.stderr);
    assert!(
        err.starts_with("bohio: ") && err.contains("service name"),
        "{err}"
    );
    assert!(output.stdout.is_empty());
}

/// Whether `bohio serve --config LAYER...` starts serving (it is then stopped), or else what it said
/// on standard error when it gave up.
fn serves(layers: &[&Path]) -> Result<(), String> {
    use std::io::Read;
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    /// Killed and waited for however the function ends.
    struct Reaped(std::process::Child);
    impl Drop for Reaped {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Reaped(
        Command::new(env!("CARGO_BIN_EXE_bohio"))
            .arg("serve")
            .args(
                layers
                    .iter()
                    .flat_map(|layer| ["--config".into(), layer.as_os_str().to_owned()]),
            )
            .args(["--listen", "tcp://127.0.0.1:0"])
            .env_remove("BOHIO_CONFIG")
            .env_remove("BOHIO_LOG")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("the binary runs"),
    );
    let mut stdout = child.0.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut first = [0_u8; 9];
        stdout.read_exact(&mut first).map(|()| first.to_vec())
    });
    let until = Instant::now() + Duration::from_secs(60);
    loop {
        if reader.is_finished() {
            let first = reader.join().unwrap();
            let _ = child.0.kill();
            let status = child.0.wait().unwrap();
            if first.as_deref().is_ok_and(|bytes| bytes == b"listening") {
                return Ok(());
            }
            let mut err = String::new();
            child
                .0
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut err)
                .unwrap();
            return Err(format!("{status}: {err}"));
        }
        if Instant::now() >= until {
            panic!("bohio serve neither served nor gave up");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(windows)]
fn icacls(path: &Path, args: &[&str]) {
    let done = Command::new("icacls")
        .arg(path)
        .args(args)
        .output()
        .expect("icacls runs");
    assert!(
        done.status.success(),
        "icacls {args:?}: {}",
        text(&done.stdout)
    );
}

/// A folder made under `%ProgramData%` lets every local user add files to it and read it, so a
/// configuration or a data directory there would let any of them choose who the host trusts, or read
/// its key. `serve` refuses either when every user may change it, or read the data directory.
#[cfg(windows)]
#[test]
fn serve_refuses_a_configuration_or_data_directory_every_user_may_reach() {
    let directory = scratch("access");
    let data = directory.join("data");
    let config = directory.join("host.cfg");
    std::fs::write(
        &config,
        format!("[service]\ndata_dir = '{}'\n", data.display()),
    )
    .unwrap();
    serves(&[&config]).expect("a private configuration and data directory are served");

    icacls(&config, &["/grant", "*S-1-5-32-545:(M)"]);
    let refused = serves(&[&config]).expect_err("a configuration every user may change is served");
    assert!(
        refused.contains("configuration file") && refused.contains("S-1-5-32-545"),
        "{refused}"
    );
    icacls(&config, &["/reset"]);
    serves(&[&config]).expect("the configuration is private again");

    icacls(&data, &["/grant", "*S-1-5-32-545:(OI)(CI)(RX)"]);
    let refused = serves(&[&config]).expect_err("a data directory every user may read is served");
    assert!(
        refused.contains("data directory") && refused.contains("S-1-5-32-545"),
        "{refused}"
    );
    icacls(&data, &["/reset", "/T"]);
    std::fs::remove_dir_all(directory).unwrap();
}

/// A layer every user may change decides who the host accepts, whichever layer it is.
#[cfg(windows)]
#[test]
fn serve_refuses_a_second_layer_every_user_may_change() {
    let directory = scratch("access-layer");
    let data = directory.join("data");
    let system = directory.join("system.cfg");
    let user = directory.join("user.cfg");
    std::fs::write(
        &system,
        format!("[service]\ndata_dir = '{}'\n", data.display()),
    )
    .unwrap();
    std::fs::write(&user, "# the user layer\n").unwrap();
    serves(&[&system, &user]).expect("two private layers are served");
    icacls(&user, &["/grant", "*S-1-5-32-545:(M)"]);
    let refused = serves(&[&system, &user]).expect_err("a layer every user may change is served");
    assert!(
        refused.contains("configuration file") && refused.contains("user.cfg"),
        "{refused}"
    );
    icacls(&user, &["/reset"]);
    icacls(&data, &["/reset", "/T"]);
    std::fs::remove_dir_all(directory).unwrap();
}

/// A layer, or the directory that holds it, that the group or others may write decides who the host
/// accepts; `check_access = false` in the lowest layer starts it anyway.
#[cfg(unix)]
#[test]
fn serve_refuses_a_layer_or_directory_the_group_or_others_may_write() {
    use std::os::unix::fs::PermissionsExt;
    let directory = scratch("access-unix");
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    let data = directory.join("data");
    let config = directory.join("host.cfg");
    let text = format!("[service]\ndata_dir = '{}'\n", data.display());
    std::fs::write(&config, &text).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    serves(&[&config]).expect("a private layer is served");

    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o660)).unwrap();
    let refused = serves(&[&config]).expect_err("a group-writable layer is served");
    assert!(
        refused.contains("group or by others") && refused.contains("host.cfg"),
        "{refused}"
    );
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();

    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o770)).unwrap();
    let refused = serves(&[&config]).expect_err("a layer in a group-writable directory is served");
    assert!(refused.contains("group or by others"), "{refused}");

    // The lowest layer may say the operator vouches for it.
    std::fs::write(&config, format!("{text}check_access = false\n")).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o660)).unwrap();
    serves(&[&config]).expect("check_access = false serves it");
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::remove_dir_all(directory).unwrap();
}

/// A child's environment for finding the user's and the system's files: only what the platform reads.
fn locations(user_root: &Path, system_root: &Path) -> Vec<(&'static str, PathBuf)> {
    if cfg!(windows) {
        vec![
            ("APPDATA", user_root.to_owned()),
            ("PROGRAMDATA", system_root.to_owned()),
        ]
    } else {
        vec![("XDG_CONFIG_HOME", user_root.to_owned())]
    }
}

fn bohio_in(args: &[&str], environment: &[(&str, PathBuf)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bohio"));
    command
        .args(args)
        .env_remove("BOHIO_CONFIG")
        .env_remove("BOHIO_LOG");
    for (name, value) in environment {
        command.env(name, value);
    }
    command.output().expect("the binary runs")
}

fn write_layer(root: &Path, text: &str) -> PathBuf {
    let path = root.join("bohio").join("bohio.cfg");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, text).unwrap();
    path
}

fn checked(output: &Output) -> serde_json::Value {
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    serde_json::from_slice(&output.stdout).expect("one JSON document")
}

#[test]
fn the_user_file_is_found_where_the_platform_keeps_it_and_merged_over_the_system_one() {
    let directory = scratch("discovery");
    let (user_root, system_root) = (directory.join("user"), directory.join("system"));
    let user = write_layer(&user_root, "[service]\nname = \"user\"\n");
    let system = write_layer(
        &system_root,
        "[service]\nname = \"system\"\n[link]\nmax_streams = 3\n",
    );
    let environment = locations(&user_root, &system_root);

    let report = checked(&bohio_in(
        &["config", "check", "--format", "json"],
        &environment,
    ));
    let files: Vec<&str> = report["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    let (user_text, system_text) = (
        user.to_string_lossy().into_owned(),
        system.to_string_lossy().into_owned(),
    );
    assert_eq!(files.last().copied(), Some(user_text.as_str()), "{files:?}");
    assert_eq!(
        report["service"], "user",
        "the user's file is merged over the system's"
    );
    if cfg!(windows) {
        assert_eq!(files, [system_text.as_str(), user_text.as_str()]);
    }

    // `--config` names the layers alone: neither location is read.
    let named = directory.join("named.cfg");
    std::fs::write(&named, "[service]\nname = \"named\"\n").unwrap();
    let report = checked(&bohio_in(
        &[
            "config",
            "check",
            "--format",
            "json",
            "--config",
            &named.to_string_lossy(),
        ],
        &environment,
    ));
    assert_eq!(report["files"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(report["service"], "named");

    // So does BOHIO_CONFIG.
    let mut by_variable = environment.clone();
    by_variable.push(("BOHIO_CONFIG", named.clone()));
    let mut command = Command::new(env!("CARGO_BIN_EXE_bohio"));
    command
        .args(["config", "check", "--format", "json"])
        .env_remove("BOHIO_LOG");
    for (name, value) in &by_variable {
        command.env(name, value);
    }
    let report = checked(&command.output().unwrap());
    assert_eq!(report["service"], "named", "{report}");
    std::fs::remove_dir_all(directory).unwrap();
}

#[cfg(windows)]
#[test]
fn user_layer_false_in_the_system_file_leaves_the_user_file_out() {
    let directory = scratch("user-layer");
    let (user_root, system_root) = (directory.join("user"), directory.join("system"));
    write_layer(&user_root, "[service]\nname = \"user\"\n");
    write_layer(
        &system_root,
        "[service]\nname = \"system\"\nuser_layer = false\n",
    );
    let report = checked(&bohio_in(
        &["config", "check", "--format", "json"],
        &locations(&user_root, &system_root),
    ));
    assert_eq!(report["service"], "system", "{report}");
    assert_eq!(report["files"].as_array().unwrap().len(), 1);
    std::fs::remove_dir_all(directory).unwrap();
}

/// Every command reads the system file, so one every user may change would choose any user's data directory
/// (where its key is made), its control endpoint and the peers it trusts. Each command refuses it, unless the
/// file says `check_access = false`: a file the operator owns may vouch for itself.
#[cfg(windows)]
#[test]
fn every_command_refuses_a_system_file_every_user_may_change() {
    let directory = scratch("system-open");
    let (user_root, system_root) = (directory.join("user"), directory.join("system"));
    std::fs::create_dir_all(&user_root).unwrap();
    let planted_data = directory.join("planted-data");
    let system = write_layer(
        &system_root,
        &format!("[service]\ndata_dir = '{}'\n", planted_data.display()),
    );
    let environment = locations(&user_root, &system_root);
    let id = || bohio_in(&["id"], &environment);
    let done = id();
    assert_eq!(done.status.code(), Some(0), "{}", text(&done.stderr));

    icacls(
        system.parent().unwrap(),
        &["/grant", "*S-1-5-32-545:(OI)(CI)M"],
    );
    std::fs::remove_dir_all(&planted_data).unwrap();
    let refused = id();
    assert_ne!(
        refused.status.code(),
        Some(0),
        "a system file every user may change chose the data directory: {}",
        text(&refused.stdout)
    );
    assert!(
        text(&refused.stderr).contains("S-1-5-32-545"),
        "{}",
        text(&refused.stderr)
    );
    assert!(
        !planted_data.exists(),
        "the key was made where the planted file said"
    );

    std::fs::write(
        &system,
        format!(
            "[service]\ndata_dir = '{}'\ncheck_access = false\n",
            planted_data.display()
        ),
    )
    .unwrap();
    let vouched = id();
    assert_eq!(vouched.status.code(), Some(0), "{}", text(&vouched.stderr));
    icacls(&directory, &["/reset", "/T"]);
    std::fs::remove_dir_all(directory).unwrap();
}

/// A peer id, from a data directory of its own.
fn made_id(directory: &Path, name: &str) -> String {
    let config = directory.join(format!("{name}.cfg"));
    std::fs::write(
        &config,
        format!(
            "[service]\ndata_dir = '{}'\n",
            directory.join(name).display()
        ),
    )
    .unwrap();
    let output = bohio(&["id", "--config", &config.to_string_lossy()]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output.stderr));
    text(&output.stdout).trim().to_owned()
}

fn peer(args: &[&str], layers: &[&Path]) -> Output {
    let mut all = vec!["peer"];
    all.extend_from_slice(args);
    let layers: Vec<String> = layers
        .iter()
        .flat_map(|l| ["--config".to_owned(), l.to_string_lossy().into_owned()])
        .collect();
    all.extend(layers.iter().map(String::as_str));
    bohio(&all)
}

#[test]
fn approve_keeps_every_other_line_of_the_file_byte_for_byte() {
    let directory = scratch("approve-keeps");
    let (first, second) = (made_id(&directory, "first"), made_id(&directory, "second"));
    let config = directory.join("host.cfg");
    let before = format!(
        "#!/usr/bin/env bohio\n# who runs this host\n[service]\nname   =  \"kept\"   # spacing and this comment stay\ndata_dir = '{}'\n\n# the first peer\n[peer.\"{first}\"]\nallow = [\"info\"]   # may ask\n\n[link]\nrecheck = \"2s\"\n",
        directory.join("host-data").display()
    );
    std::fs::write(&config, &before).unwrap();

    let done = peer(&["approve", &second, "--allow", "exec"], &[&config]);
    assert_eq!(done.status.code(), Some(0), "{}", text(&done.stderr));
    assert!(
        text(&done.stdout).contains(&config.to_string_lossy().into_owned()),
        "it says which file it wrote: {}",
        text(&done.stdout)
    );
    let after = std::fs::read_to_string(&config).unwrap();
    // Every old line is still there, in its order: the new table is added, nothing is rewritten.
    let mut rest = after.as_str();
    for line in before.lines() {
        let at = rest
            .find(line)
            .unwrap_or_else(|| panic!("`{line}` was lost or moved:\n{after}"));
        rest = &rest[at + line.len()..];
    }
    assert!(
        after.contains(&format!("[peer.\"{second}\"]")) && after.contains("allow = [\"exec\"]"),
        "{after}"
    );

    // Replacing an existing table's grant keeps the lines around it.
    let done = peer(
        &[
            "approve",
            &first,
            "--allow",
            "info,fs.read",
            "--fs-root",
            "/srv",
        ],
        &[&config],
    );
    assert_eq!(done.status.code(), Some(0), "{}", text(&done.stderr));
    let replaced = std::fs::read_to_string(&config).unwrap();
    for line in before
        .lines()
        .filter(|line| !line.starts_with("allow = [\"info\"]"))
    {
        assert!(replaced.contains(line), "`{line}` was lost:\n{replaced}");
    }
    assert!(
        replaced.contains("fs.read") && replaced.contains("/srv"),
        "{replaced}"
    );

    // Approving an id that has a table and no flags changes nothing.
    let unchanged = std::fs::read(&config).unwrap();
    let done = peer(&["approve", &first], &[&config]);
    assert_eq!(done.status.code(), Some(0), "{}", text(&done.stderr));
    assert_eq!(std::fs::read(&config).unwrap(), unchanged);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn approve_writes_to_the_layer_that_holds_the_peer_else_the_last_one_it_can_write() {
    let directory = scratch("approve-target");
    let (held, fresh) = (made_id(&directory, "held"), made_id(&directory, "fresh"));
    let system = directory.join("system.cfg");
    let user = directory.join("user.cfg");
    std::fs::write(&system, format!("[peer.\"{held}\"]\nallow = [\"info\"]\n")).unwrap();
    std::fs::write(&user, "# user layer\n").unwrap();
    let layers: [&Path; 2] = [&system, &user];

    peer(&["approve", &held, "--allow", "exec"], &layers);
    assert!(
        std::fs::read_to_string(&system).unwrap().contains("exec"),
        "the layer that holds the table is changed"
    );
    assert_eq!(std::fs::read_to_string(&user).unwrap(), "# user layer\n");

    peer(&["approve", &fresh], &layers);
    assert!(
        std::fs::read_to_string(&user).unwrap().contains(&fresh),
        "a new peer goes to the last layer"
    );
    assert!(!std::fs::read_to_string(&system).unwrap().contains(&fresh));

    // A last layer this account cannot write is skipped; with none writable there is no target.
    let other = made_id(&directory, "other");
    read_only(&user, true);
    // (A superuser can write anything: the check below is for an ordinary account.)
    let writable_anyway = std::fs::OpenOptions::new().append(true).open(&user).is_ok();
    if !writable_anyway {
        let done = peer(&["approve", &other], &layers);
        assert_eq!(done.status.code(), Some(0), "{}", text(&done.stderr));
        assert!(
            std::fs::read_to_string(&system).unwrap().contains(&other),
            "it fell back to the system layer"
        );
        read_only(&system, true);
        let none = peer(&["approve", &made_id(&directory, "third")], &layers);
        assert_eq!(none.status.code(), Some(1));
        assert!(
            text(&none.stderr).contains("no configuration file can be written"),
            "{}",
            text(&none.stderr)
        );
        read_only(&system, false);
    }
    read_only(&user, false);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn approve_makes_the_user_file_when_no_layer_exists() {
    let directory = scratch("approve-new");
    let (user_root, system_root) = (directory.join("user"), directory.join("system"));
    let id = made_id(&directory, "peer");
    let done = bohio_in(
        &["peer", "approve", &id, "--allow", "info"],
        &locations(&user_root, &system_root),
    );
    assert_eq!(done.status.code(), Some(0), "{}", text(&done.stderr));
    let made = user_root.join("bohio").join("bohio.cfg");
    assert!(
        std::fs::read_to_string(&made).unwrap().contains(&id),
        "the user file holds the peer"
    );
    assert!(
        text(&done.stdout).contains("bohio.cfg"),
        "{}",
        text(&done.stdout)
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&made).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn revoke_removes_the_table_from_every_layer_that_holds_it_and_names_one_it_cannot_write() {
    let directory = scratch("revoke");
    let id = made_id(&directory, "peer");
    let table = format!(
        "# kept comment\n[service]\nname = \"kept\"\n\n[peer.\"{id}\"]\nallow = [\"info\"]\n"
    );
    let (system, user) = (directory.join("system.cfg"), directory.join("user.cfg"));
    std::fs::write(&system, &table).unwrap();
    std::fs::write(&user, &table).unwrap();
    let done = peer(&["revoke", &id], &[&system, &user]);
    assert_eq!(done.status.code(), Some(0), "{}", text(&done.stderr));
    for file in [&system, &user] {
        let left = std::fs::read_to_string(file).unwrap();
        assert!(
            !left.contains(&id) && left.contains("# kept comment"),
            "{left}"
        );
    }
    let listed = text(&peer(&["list"], &[&system, &user]).stdout);
    assert!(listed.contains("no peers"), "{listed}");

    // A layer that cannot be written keeps the key accepted: the command says which.
    std::fs::write(&system, &table).unwrap();
    std::fs::write(&user, &table).unwrap();
    read_only(&system, true);
    if std::fs::OpenOptions::new()
        .append(true)
        .open(&system)
        .is_err()
    {
        let done = peer(&["revoke", &id], &[&system, &user]);
        assert_eq!(done.status.code(), Some(0), "{}", text(&done.stderr));
        let out = text(&done.stdout);
        assert!(
            out.contains("cannot write") && out.contains("system.cfg"),
            "{out}"
        );
        assert!(!std::fs::read_to_string(&user).unwrap().contains(&id));
        assert!(std::fs::read_to_string(&system).unwrap().contains(&id));
    }
    read_only(&system, false);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn a_peer_with_no_table_is_not_found_to_revoke() {
    let directory = scratch("revoke-none");
    let id = made_id(&directory, "peer");
    let config = directory.join("host.cfg");
    std::fs::write(&config, "[service]\nname = \"x\"\n").unwrap();
    let done = peer(&["revoke", &id], &[&config]);
    assert_eq!(done.status.code(), Some(1));
    assert!(
        text(&done.stderr).contains("no accepted or waiting peer"),
        "{}",
        text(&done.stderr)
    );
    std::fs::remove_dir_all(directory).unwrap();
}
