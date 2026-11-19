//! This host's capabilities, as `info` reports them.

use std::path::Path;

use bohio_proto::PROTOCOL_VERSION;
use bohio_proto::info::{ExecInfo, FilesystemInfo, Info, Limits, Service, SessionInfo, ShellInfo};
use bohio_proto::shell::ShellKind;

use crate::config::{Config, Environment};
use crate::platform::find_program;

/// What this host offers under `config`: the operating system, the shells found on
/// `environment.path`, what the filesystem supports and the configured bounds.
///
/// `host_id` stays empty: a link fills it once it has proven an identity.
pub fn collect(config: &Config, environment: &Environment) -> Info {
    let path = environment.path.clone().unwrap_or_default();
    let shells = ShellKind::KNOWN
        .into_iter()
        .filter_map(|kind| {
            find_program(kind.program(), &path).map(|found| ShellInfo {
                kind,
                path: display(&found),
            })
        })
        .collect();
    Info {
        protocol: PROTOCOL_VERSION,
        service: Service {
            name: config.service.name.clone(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
        },
        host_id: None,
        os: std::env::consts::OS.to_owned(),
        arch: std::env::consts::ARCH.to_owned(),
        shells,
        filesystem: FilesystemInfo {
            enabled: config.filesystem.enabled,
            symlinks: true,
            unix_modes: cfg!(unix),
            windows_attributes: cfg!(windows),
            case_sensitive: !cfg!(any(windows, target_os = "macos")),
            separator: std::path::MAIN_SEPARATOR.to_string(),
        },
        exec: ExecInfo {
            enabled: config.execution.enabled,
        },
        shell: SessionInfo {
            enabled: config.shell.enabled,
        },
        limits: Limits {
            max_streams: Some(config.link.max_streams as u64),
            max_processes: Some(config.execution.max_processes as u64),
            max_sessions: Some(config.shell.max_sessions as u64),
        },
    }
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
