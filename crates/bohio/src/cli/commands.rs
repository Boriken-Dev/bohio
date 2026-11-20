//! What each command does and how it prints.

use std::path::Path;
use std::process::ExitCode;

use bohio_proto::info::Info;

use super::{
    CheckArgs, Command, ConfigAction, ConfigOption, Format, InfoArgs, host, remote, service,
};
use crate::config::{Config, Environment};
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
    }
}

pub(super) fn load(
    explicit: Option<&Path>,
    environment: &Environment,
) -> Result<(Config, Option<std::path::PathBuf>), String> {
    Config::locate(explicit, environment).map_err(|error| error.to_string())
}

/// The configuration and the data directory it names.
pub(super) fn source(
    option: &ConfigOption,
    environment: &Environment,
) -> Result<(Config, Data), String> {
    let (config, _) = load(option.config.as_deref(), environment)?;
    let data = Data::new(config.data_dir(environment));
    Ok((config, data))
}

fn config_check(args: &CheckArgs, environment: &Environment) -> Result<(), String> {
    let (config, from) = load(args.config.as_deref(), environment)?;
    let source = from.as_ref().map(|p| p.display().to_string());
    match args.format {
        Format::Text => {
            match &source {
                Some(path) => println!("ok: {path}"),
                None => println!("ok: no configuration file found; the defaults apply"),
            }
            println!("service: {}", config.service.name);
            println!("data_dir: {}", config.data_dir(environment).display());
            println!("listen: {}", config.listen.len());
            println!("filesystem: {}", enabled(config.filesystem.enabled));
            println!("execution: {}", enabled(config.execution.enabled));
            println!("shell: {}", enabled(config.shell.enabled));
        }
        Format::Json => {
            let report = serde_json::json!({
                "ok": true,
                "file": source,
                "service": config.service.name,
                "data_dir": config.data_dir(environment).display().to_string(),
                "listen": config.listen.iter().map(|l| l.endpoint.as_str()).collect::<Vec<_>>(),
                "filesystem": config.filesystem.enabled,
                "execution": config.execution.enabled,
                "shell": config.shell.enabled,
            });
            println!("{report}");
        }
    }
    Ok(())
}

fn info_command(args: &InfoArgs, environment: &Environment) -> Result<(), String> {
    let (config, _) = load(args.config.as_deref(), environment)?;
    let info = info::collect(
        &config.node(),
        &environment.path.clone().unwrap_or_default(),
    );
    match args.format {
        Format::Text => print_info(&info),
        Format::Json => {
            println!(
                "{}",
                serde_json::to_string(&info).map_err(|_| "the capabilities cannot be written")?
            );
        }
    }
    Ok(())
}

const fn enabled(on: bool) -> &'static str {
    if on { "enabled" } else { "disabled" }
}

pub(super) fn print_info(info: &Info) {
    println!("service: {} {}", info.service.name, info.service.version);
    println!("protocol: {}", info.protocol);
    println!("os: {} {}", info.os, info.arch);
    for shell in &info.shells {
        println!("shell: {:?} {}", shell.kind, shell.path);
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
    println!("exec: {}", enabled(info.exec.enabled));
    println!("shell sessions: {}", enabled(info.shell.enabled));
    let limits = &info.limits;
    println!(
        "limits: max_streams={} max_processes={} max_sessions={}",
        limit(limits.max_streams),
        limit(limits.max_processes),
        limit(limits.max_sessions)
    );
}

fn limit(value: Option<u64>) -> String {
    value.map_or_else(|| "none".to_owned(), |v| v.to_string())
}
