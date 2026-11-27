//! What each command does and how it prints.

use std::process::ExitCode;

use bohio_proto::info::Info;

use super::{
    CheckArgs, Command, ConfigAction, ConfigOption, HostsArgs, InfoArgs, gui, gui_helper, host,
    mcp, output, remote, service,
};
use crate::config::{Environment, Found, discover};
use crate::info;
use crate::peer::Data;

/// Runs one command; the error is one line for standard error.
pub(super) fn run(command: &Command, environment: &Environment) -> Result<ExitCode, String> {
    let done = |result: Result<(), String>| result.map(|()| ExitCode::SUCCESS);
    match command {
        Command::Config(args) => match &args.action {
            ConfigAction::Check(check) => done(config_check(check, environment)),
        },
        Command::Info(args) => done(info_command(args, environment)),
        Command::Serve(args) => done(host::serve(args, environment)),
        Command::Status(args) => done(host::status(args, environment)),
        Command::Id(args) => done(host::id(args, environment)),
        Command::Peer(args) => done(host::peer(args, environment)),
        Command::Pair(args) => done(host::pair(args, environment)),
        Command::Join(args) => done(host::join(args, environment)),
        Command::Service(args) => done(service::run(args, environment)),
        Command::Remote(args) => remote::run(args, environment),
        Command::Links(args) => done(remote::links(args, environment)),
        Command::Providers(args) => done(remote::providers(args, environment)),
        Command::Hosts(args) => done(hosts_command(args, environment)),
        Command::Gui(args) => done(gui::approve(args, environment)),
        Command::GuiHelper(args) => done(gui_helper::run(args, environment)),
        Command::Mcp(args) => mcp::run(args, environment),
    }
}

/// The configuration and the data directory it names.
pub(super) fn source(
    option: &ConfigOption,
    environment: &Environment,
) -> Result<(Found, Data), String> {
    let found = discover(&option.config).map_err(|error| error.to_string())?;
    let data = Data::new(
        found
            .config()
            .data_dir(environment)
            .map_err(|error| error.to_string())?,
    )
    .with_waiting(&found.config().waiting);
    Ok((found, data))
}

fn config_check(args: &CheckArgs, environment: &Environment) -> Result<(), String> {
    let found = discover(&args.config.config).map_err(|error| error.to_string())?;
    let config = found.config();
    let files: Vec<String> = found
        .layered
        .files
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    let data_dir = config
        .data_dir(environment)
        .map_err(|error| error.to_string())?;
    if output::json() {
        output::object(&serde_json::json!({
            "ok": true,
            "files": files,
            "service": config.service.name,
            "data_dir": data_dir.display().to_string(),
            "listen": config.listen.iter().map(|l| l.endpoint.as_str()).collect::<Vec<_>>(),
            "peers": config.peer.len(),
            "dial": config.peer.dials().map(|(id, endpoint, _)| serde_json::json!({ "peer": id, "endpoint": endpoint })).collect::<Vec<_>>(),
            "hosts": config.remote.names().collect::<Vec<_>>(),
            "filesystem": config.filesystem.enabled,
            "execution": config.execution.enabled,
            "shell": config.shell.enabled,
            "profile": found.layered.profile.name(),
            "transfer": config.transfer_keys().into_iter().map(|(key, value)| serde_json::json!({
                "key": key,
                "value": value,
                "source": found.layered.origin(key).to_string(),
            })).collect::<Vec<_>>(),
        }));
        return Ok(());
    }
    if files.is_empty() {
        println!("ok: no configuration file found; the defaults apply");
    } else {
        println!("ok: {}", files.join(", "));
    }
    println!("service: {}", config.service.name);
    println!("data_dir: {}", data_dir.display());
    println!("listen: {}", config.listen.len());
    println!("peers: {}", config.peer.len());
    println!("dial: {}", config.peer.dials().count());
    println!("hosts: {}", config.remote.len());
    println!("filesystem: {}", enabled(config.filesystem.enabled));
    println!("execution: {}", enabled(config.execution.enabled));
    println!("shell: {}", enabled(config.shell.enabled));
    println!("profile: {}", found.layered.profile.name());
    for (key, value) in config.transfer_keys() {
        println!("{key} = {value} ({})", found.layered.origin(key));
    }
    Ok(())
}

/// `bohio hosts`: the hosts `[remote]` names, one line each, or `{"hosts": [...]}`.
fn hosts_command(args: &HostsArgs, environment: &Environment) -> Result<(), String> {
    let (found, _) = source(&args.config, environment)?;
    let hosts = &found.config().remote;
    if output::json() {
        output::object(&serde_json::json!({
            "hosts": hosts.iter().map(|(name, host)| serde_json::json!({
                "name": name,
                "endpoint": host.endpoint,
                "server": host.server,
                "ca": host.ca.iter().map(|file| file.display().to_string()).collect::<Vec<_>>(),
                "cert_hash": host.cert_hash,
                "fallback": host.fallback,
                "jump": host.jump,
            })).collect::<Vec<_>>(),
        }));
        return Ok(());
    }
    if hosts.is_empty() {
        println!("no hosts named: add a [remote.NAME] table with an `endpoint` to bohio.cfg");
    }
    for (name, host) in hosts.iter() {
        let mut line = format!("{name} {}", host.endpoint);
        if let Some(server) = &host.server {
            line.push_str(&format!(" server={server}"));
        }
        if !host.jump.is_empty() {
            line.push_str(&format!(" jump={}", host.jump.join(",")));
        }
        println!("{line}");
    }
    Ok(())
}

fn info_command(args: &InfoArgs, environment: &Environment) -> Result<(), String> {
    let found = discover(&args.config.config).map_err(|error| error.to_string())?;
    let info = info::collect(
        &super::host::node_settings(found.config(), environment)?,
        &environment.path.clone().unwrap_or_default(),
    );
    if output::json() {
        println!(
            "{}",
            serde_json::to_string(&info).map_err(|_| "the capabilities cannot be written")?
        );
    } else {
        print_info(&info);
    }
    Ok(())
}

const fn enabled(on: bool) -> &'static str {
    if on { "enabled" } else { "disabled" }
}

/// The signals as a comma-separated list, or `none`.
fn signal_names(signals: &[bohio_proto::signal::Signal]) -> String {
    if signals.is_empty() {
        "none".to_owned()
    } else {
        signals
            .iter()
            .map(|signal| signal.name())
            .collect::<Vec<_>>()
            .join(",")
    }
}

pub(super) fn print_info(info: &Info) {
    println!(
        "service: {} {}",
        output::shown(&info.service.name),
        output::shown(&info.service.version)
    );
    println!("protocol: {}", info.protocol);
    println!(
        "os: {} {}",
        output::shown(&info.os),
        output::shown(&info.arch)
    );
    for shell in &info.shells {
        println!("shell: {:?} {}", shell.kind, output::shown(&shell.path));
    }
    let fs = &info.filesystem;
    println!(
        "filesystem: {} symlinks={} unix_modes={} windows_attributes={} case_sensitive={} separator={}",
        enabled(fs.enabled),
        fs.symlinks,
        fs.unix_modes,
        fs.windows_attributes,
        fs.case_sensitive,
        fs.separator
    );
    println!(
        "exec: {} signals={}",
        enabled(info.exec.enabled),
        signal_names(&info.exec.signals)
    );
    println!(
        "shell sessions: {} signals={}",
        enabled(info.shell.enabled),
        signal_names(&info.shell.signals)
    );
    println!(
        "detached sessions: {} (keeps {} bytes, up to {}; linger {}s)",
        enabled(info.sessions.enabled),
        info.sessions.scrollback,
        info.sessions.max_scrollback,
        info.sessions.linger_ms / 1000
    );
    let limits = &info.limits;
    println!(
        "limits: max_streams={} max_processes={} max_sessions={}",
        limit(limits.max_streams),
        limit(limits.max_processes),
        limit(limits.max_sessions)
    );
    let transfer = &info.transfer;
    let names: Vec<&str> = transfer.hashes.iter().map(|name| name.name()).collect();
    println!(
        "hashes: {} (max_streams={} rate={})",
        if names.is_empty() {
            "none".to_owned()
        } else {
            names.join(",")
        },
        transfer.hash_limits.max_streams,
        if transfer.hash_limits.rate == 0 {
            "unlimited".to_owned()
        } else {
            format!("{}/s", transfer.hash_limits.rate)
        }
    );
    if transfer.compression.is_empty() {
        println!("compression: none");
    }
    for item in &transfer.compression {
        let levels = if item.levels.min == 0 && item.levels.max == 0 {
            "no levels".to_owned()
        } else {
            format!(
                "levels {}..{} (default {})",
                item.levels.min, item.levels.max, item.default_level
            )
        };
        let window = if item.window_log_max == 0 {
            String::new()
        } else {
            format!(", window up to 2^{}", item.window_log_max)
        };
        println!("compression: {} {levels}{window}", item.algorithm);
    }
    if let Some(default) = &transfer.compression_default {
        println!(
            "compression default (auto): {}{}",
            default.algorithm,
            default
                .level
                .map_or_else(String::new, |level| format!(":{level}"))
        );
    }
    println!("prepared reads: {}", enabled(transfer.prepared));
    if transfer.delta.weak.is_empty() {
        println!("deltas: off");
    } else {
        println!(
            "deltas: {} (blocks {} to {} bytes, automatic from {} bytes, signature up to {} bytes)",
            transfer.delta.weak.join(", "),
            transfer.delta.min_block,
            transfer.delta.max_block,
            transfer.delta.min_size,
            transfer.delta.max_signature_bytes
        );
    }
    println!(
        "walk: {} (max_streams={} max_entries={}), bundles: {}, resume: {}",
        enabled(transfer.walk),
        transfer.walk_limits.max_streams,
        if transfer.walk_limits.max_entries == 0 {
            "unlimited".to_owned()
        } else {
            transfer.walk_limits.max_entries.to_string()
        },
        enabled(transfer.bundles),
        enabled(transfer.resume)
    );
    let gui = &info.gui;
    if gui.enabled {
        println!(
            "gui: {}",
            gui.backend
                .as_deref()
                .or(gui.reason.as_deref())
                .unwrap_or("unavailable")
        );
    } else {
        println!("gui: disabled");
    }
    let access = &info.access;
    for (capability, sources) in [
        ("files", &access.fs),
        ("exec", &access.exec),
        ("terminal", &access.terminal),
        ("desktop", &access.desktop),
        ("power", &access.power),
        ("media", &access.media),
    ] {
        for source in sources {
            println!(
                "source {capability} {} provider={} default={} available={} lacks=[{}]{}",
                source.name,
                source.provider,
                source.default,
                source.available,
                source.lacks.join(","),
                source
                    .reason
                    .as_deref()
                    .map_or_else(String::new, |reason| format!(" ({reason})"))
            );
        }
    }
}

fn limit(value: Option<u64>) -> String {
    value.map_or_else(|| "none".to_owned(), |v| v.to_string())
}
