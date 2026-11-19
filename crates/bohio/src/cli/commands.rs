//! What each command does and how it prints.

use std::path::Path;

use bohio_proto::info::Info;

use super::{CheckArgs, Command, ConfigAction, Format, InfoArgs};
use crate::config::{Config, Environment};
use crate::info;

/// Runs one command; the error is one line for standard error.
pub(super) fn run(command: &Command, environment: &Environment) -> Result<(), String> {
    match command {
        Command::Config(args) => match &args.action {
            ConfigAction::Check(check) => config_check(check, environment),
        },
        Command::Info(args) => info_command(args, environment),
    }
}

fn load(
    explicit: Option<&Path>,
    environment: &Environment,
) -> Result<(Config, Option<std::path::PathBuf>), String> {
    Config::locate(explicit, environment).map_err(|error| error.to_string())
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
    let info = info::collect(&config, environment);
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

fn print_info(info: &Info) {
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
