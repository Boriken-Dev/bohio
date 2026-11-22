//! This host's capabilities, as `info` reports them.

use std::ffi::OsStr;
use std::path::Path;

use bohio_proto::PROTOCOL_VERSION;
use bohio_proto::forward::ForwardInfo;
use bohio_proto::info::{
    ExecInfo, FilesystemInfo, HashLimits, Info, Limits, Service, SessionInfo, ShellInfo,
    TransferInfo,
};
use bohio_proto::shell::ShellKind;

use crate::config::NodeConfig;
use crate::platform::find_program;

/// What this host offers under `config`: the operating system, the shells found on `path` (the host's
/// own `PATH`), what the filesystem supports and the configured bounds.
///
/// `host_id` stays empty: a link fills it once it has proven an identity.
pub fn collect(config: &NodeConfig, path: &OsStr) -> Info {
    let shells = ShellKind::KNOWN
        .into_iter()
        .filter_map(|kind| {
            find_program(kind.program(), path).map(|found| ShellInfo {
                kind,
                path: display(&found),
            })
        })
        .collect();
    Info {
        protocol: PROTOCOL_VERSION,
        service: Service {
            name: config.name.clone(),
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
        transfer: TransferInfo {
            hashes: if config.filesystem.enabled {
                config.hash.algorithms.clone()
            } else {
                Vec::new()
            },
            hash_limits: HashLimits {
                max_streams: config.hash.max_streams as u64,
                rate: config.hash.rate,
            },
            compression: {
                let caps = config.compression.caps();
                config
                    .compression
                    .allowed
                    .iter()
                    .filter_map(|codec| caps.describe(codec))
                    .collect()
            },
            compression_default: config
                .compression
                .allowed
                .contains(&config.compression.default.algorithm)
                .then(|| caps_default(&config.compression)),
            prepared: config.filesystem.enabled && config.compression.prepare_dir.is_some(),
        },
        forward: ForwardInfo {
            enabled: config.forward.enabled,
            connect: if config.forward.enabled {
                crate::forward::connect_kinds()
            } else {
                Vec::new()
            },
            listen: if config.forward.enabled {
                crate::forward::listen_kinds()
            } else {
                Vec::new()
            },
        },
        gui: bohio_proto::gui::GuiInfo {
            enabled: config.gui.enabled,
            formats: config.gui.formats.clone(),
            ..Default::default()
        },
    }
}

/// What `auto` means, after the caps: the level the host would use.
fn caps_default(config: &crate::config::CompressionConfig) -> bohio_proto::compress::Compression {
    config
        .caps()
        .clamp(&config.default)
        .unwrap_or_else(|_| config.default.clone())
}

fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
