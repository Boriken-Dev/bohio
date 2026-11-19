//! The built `bohio` binary, run with real arguments.

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

fn scratch(name: &str) -> PathBuf {
    let directory = std::env::temp_dir().join(format!("bohio-cli-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    directory
}
