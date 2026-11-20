//! What each command does and how it prints.

use std::path::PathBuf;
use std::process::ExitCode;

use bohio_proto::info::Info;

use super::{
    CheckArgs, Command, ConfigAction, ConfigOption, Format, InfoArgs, host, remote, service,
};
use crate::config::{Config, Environment, Layered, Layers};
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

/// What the configuration layers named, or found, come to.
pub(super) struct Found {
    /// The merged configuration, and the layer files read.
    pub layered: Layered,
    /// The layer files to ask again about the peers: the ones named, or the system file and the user's
    /// whether or not each exists yet (one made later by `bohio peer approve` then applies).
    pub watch: Vec<PathBuf>,
    /// The user file to make when `approve` finds no layer it can write.
    pub create: Option<PathBuf>,
}

impl Found {
    /// The merged configuration.
    pub(super) fn config(&self) -> &Config {
        &self.layered.config
    }
}

/// The configuration file under the system and user directories: `bohio/bohio.cfg`.
fn name() -> PathBuf {
    ["bohio", "bohio.cfg"].iter().collect()
}

/// The system and user files by their usual places.
fn defaults() -> (PathBuf, Option<PathBuf>) {
    let name = name();
    (
        anycfg::system_config_path(Some(&name)),
        anycfg::user_config_path(Some(&name)).ok(),
    )
}

/// The layers `named` names (alone, lowest first); else the system file with the user's merged over
/// it. `[service] user_layer = false` in the lowest layer leaves the user's file out. Files that do not
/// exist are not layers; none at all is the defaults.
pub(super) fn found(named: &[PathBuf]) -> Result<Found, String> {
    let (system, user) = defaults();
    found_in(named, system, user)
}

/// As [`found`], with the system file and the user's given.
fn found_in(named: &[PathBuf], system: PathBuf, user: Option<PathBuf>) -> Result<Found, String> {
    let named: Vec<PathBuf> = named
        .iter()
        .filter(|path| !path.as_os_str().is_empty())
        .cloned()
        .collect();
    if !named.is_empty() {
        let layered = Layers::load(&named).map_err(|error| error.to_string())?;
        return Ok(Found {
            layered,
            watch: named,
            create: None,
        });
    }
    let user_layer = if system.is_file() {
        Layers::load(std::slice::from_ref(&system))
            .map_err(|error| error.to_string())?
            .config
            .service
            .user_layer
    } else {
        true
    };
    let mut watch = vec![system];
    let mut create = None;
    if user_layer && let Some(user) = user {
        create = Some(user.clone());
        watch.push(user);
    }
    let files: Vec<PathBuf> = watch
        .iter()
        .filter(|path| path.is_file())
        .cloned()
        .collect();
    let layered = Layers::load(&files).map_err(|error| error.to_string())?;
    Ok(Found {
        layered,
        watch,
        create,
    })
}

/// The configuration and the data directory it names.
pub(super) fn source(
    option: &ConfigOption,
    environment: &Environment,
) -> Result<(Found, Data), String> {
    let found = found(&option.config)?;
    let data = Data::new(found.config().data_dir(environment));
    Ok((found, data))
}

fn config_check(args: &CheckArgs, environment: &Environment) -> Result<(), String> {
    let found = found(&args.config)?;
    let config = found.config();
    let files: Vec<String> = found
        .layered
        .files
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    match args.format {
        Format::Text => {
            if files.is_empty() {
                println!("ok: no configuration file found; the defaults apply");
            } else {
                println!("ok: {}", files.join(", "));
            }
            println!("service: {}", config.service.name);
            println!("data_dir: {}", config.data_dir(environment).display());
            println!("listen: {}", config.listen.len());
            println!("peers: {}", config.peer.len());
            println!("dial: {}", config.peer.dials().count());
            println!("filesystem: {}", enabled(config.filesystem.enabled));
            println!("execution: {}", enabled(config.execution.enabled));
            println!("shell: {}", enabled(config.shell.enabled));
        }
        Format::Json => {
            let report = serde_json::json!({
                "ok": true,
                "files": files,
                "service": config.service.name,
                "data_dir": config.data_dir(environment).display().to_string(),
                "listen": config.listen.iter().map(|l| l.endpoint.as_str()).collect::<Vec<_>>(),
                "peers": config.peer.len(),
                "dial": config.peer.dials().map(|(id, endpoint, _)| serde_json::json!({ "peer": id, "endpoint": endpoint })).collect::<Vec<_>>(),
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
    let found = found(&args.config)?;
    let info = info::collect(
        &found.config().node(),
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

#[cfg(test)]
mod tests {
    use super::found_in;
    use std::path::PathBuf;

    fn file(dir: &std::path::Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn the_user_file_is_merged_over_the_system_one_unless_the_system_one_turns_it_off() {
        let dir = tempfile::tempdir().unwrap();
        let system = file(
            dir.path(),
            "system.cfg",
            "[service]
name = \"system\"
",
        );
        let user = file(
            dir.path(),
            "user.cfg",
            "[service]
name = \"user\"
",
        );
        let both = found_in(&[], system.clone(), Some(user.clone())).unwrap();
        assert_eq!(both.layered.files, [system.clone(), user.clone()]);
        assert_eq!(both.config().service.name, "user");
        assert_eq!(both.create.as_deref(), Some(user.as_path()));

        file(
            dir.path(),
            "system.cfg",
            "[service]
name = \"system\"
user_layer = false
",
        );
        let off = found_in(&[], system.clone(), Some(user.clone())).unwrap();
        assert_eq!(off.layered.files, std::slice::from_ref(&system));
        assert_eq!(off.config().service.name, "system");
        assert!(
            off.create.is_none(),
            "a user file nothing reads is not made"
        );
        assert_eq!(off.watch, std::slice::from_ref(&system));

        // Named layers are read alone, whatever the system file says and whether or not they exist.
        let named = found_in(std::slice::from_ref(&user), system, Some(user.clone())).unwrap();
        assert_eq!(named.layered.files, [user]);
        assert!(named.create.is_none());
    }

    #[test]
    fn a_missing_file_is_not_a_layer_and_no_file_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let (system, user) = (dir.path().join("none.cfg"), dir.path().join("also.cfg"));
        let found = found_in(&[], system.clone(), Some(user.clone())).unwrap();
        assert!(found.layered.files.is_empty());
        assert_eq!(found.config(), &crate::config::Config::default());
        assert_eq!(
            found.watch,
            [system, user.clone()],
            "a file made later is watched"
        );
        assert_eq!(found.create, Some(user));
    }
}
