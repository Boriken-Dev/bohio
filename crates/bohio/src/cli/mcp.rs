//! `bohio mcp`: the agent tools over the Model Context Protocol, on standard input and output.
//!
//! The server is [`crate::mcp`]; this file resolves each `--host` as `bohio remote` would, and hands the server standard input
//! and output.

use std::process::ExitCode;

use bohio_client::dial::split_command;
use clap::Parser;

use super::remote::dialer;
use super::{AfterAction, McpArgs, RemoteArgs};
use crate::config::Environment;
use crate::mcp::{Host, Options, serve};

/// One host's arguments, as `bohio remote` takes them.
#[derive(Debug, Parser)]
#[command(
    no_binary_name = true,
    disable_help_flag = true,
    disable_version_flag = true
)]
struct HostLine {
    #[command(flatten)]
    args: RemoteArgs,
}

/// A host's `[NAME=]ARGS` as its name and the words of `bohio remote` it stands for.
fn host_words(spec: &str) -> Result<(String, Vec<String>), String> {
    let plain = |text: &str| {
        !text.is_empty()
            && text
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    };
    if let Some((name, rest)) = spec.split_once('=')
        && plain(name)
        && !name.starts_with('-')
    {
        return Ok((
            name.to_owned(),
            split_command(rest).map_err(|error| error.to_string())?,
        ));
    }
    if plain(spec) && !spec.starts_with('-') {
        // A bare name: `bohio remote NAME` resolves it.
        return Ok((spec.to_owned(), vec![spec.to_owned()]));
    }
    let words = split_command(spec).map_err(|error| error.to_string())?;
    // A `[remote]` name followed by flags keeps the name; any other line is the one unnamed host.
    let name = words
        .first()
        .filter(|word| plain(word) && !word.starts_with('-'))
        .map_or_else(|| "host".to_owned(), Clone::clone);
    Ok((name, words))
}

/// Everything `bohio mcp` serves.
pub(super) fn run(args: &McpArgs, environment: &Environment) -> Result<ExitCode, String> {
    if args.host.is_empty() {
        return Err(
            "name at least one host with --host [NAME=]ENDPOINT [FLAGS of `bohio remote`]"
                .to_owned(),
        );
    }
    let mcp = super::commands::source(&args.config, environment)?
        .0
        .config()
        .mcp
        .clone();
    let mut hosts: Vec<Host> = Vec::new();
    for spec in &args.host {
        let (name, mut words) = host_words(spec)?;
        if hosts.iter().any(|host| host.name == name) {
            return Err(format!("two hosts are named `{name}`"));
        }
        // The action is the one a line needs to parse; the tools replace it.
        words.push("info".to_owned());
        let mut line = HostLine::try_parse_from(&words).map_err(|error| {
            format!(
                "--host `{name}`: {}",
                error.to_string().lines().next().unwrap_or("")
            )
        })?;
        if line.args.config.config.is_empty() {
            line.args.config.config.clone_from(&args.config.config);
        }
        let (dial, mut settings) = dialer(&line.args, environment)?;
        if let Some(bytes) = args.max_output {
            settings.output.default = bytes;
            settings.output.ceiling = settings.output.ceiling.max(bytes);
        }
        settings.local_roots.clone_from(&args.local_root);
        settings
            .forward_local_targets
            .clone_from(&args.forward_local_target);
        settings.forward_allow_remote_bind = args.forward_allow_remote_bind;
        settings.gui_auto_limit = args.gui_max_image_bytes.unwrap_or(mcp.gui_max_image_bytes);
        settings.gui_after_action = args.gui_after_action == AfterAction::Screenshot;
        hosts.push(Host {
            name,
            dial,
            settings,
        });
    }
    let options = Options::default()
        .with_max_calls(args.max_calls.unwrap_or(mcp.max_calls))
        .with_gui(args.gui);
    serve(hosts, &options, std::io::stdin().lock(), std::io::stdout())?;
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::host_words;

    #[test]
    fn a_remote_name_followed_by_flags_is_served_under_that_name() {
        let (name, words) = host_words("lab --jump tcp://j:1").unwrap();
        assert_eq!(name, "lab");
        assert_eq!(words, ["lab", "--jump", "tcp://j:1"]);
        assert_eq!(host_words("web").unwrap().0, "web");
        assert_eq!(host_words("web=tcp://h:1").unwrap().0, "web");
        // An endpoint, or a line that starts with a flag, is the unnamed host.
        assert_eq!(host_words("tcp://h:1 --server x").unwrap().0, "host");
        assert_eq!(host_words("--command 'ssh h'").unwrap().0, "host");
    }
}
