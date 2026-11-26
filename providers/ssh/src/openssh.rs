// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The system's `ssh` program as the client: one process for each terminal, program and SFTP session. It reads the account's
//! own `ssh_config` (Kerberos, `ProxyJump`, an agent of its own), and never asks a question: `BatchMode` is always on.

use std::process::Stdio;
use std::time::{Duration, Instant};

use bohio_api::guatiao::Status;
use bohio_api::guatiao::library::ProviderError;
use tokio::process::Command;

use crate::link::{Link, refuse};
use crate::settings::{HostKeyCheck, Settings};

/// What a null `known_hosts` file is called here.
const NO_KNOWN_HOSTS: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

/// The options this provider sets from the source's settings, before the account's own and `ssh_args`.
pub(crate) fn options(settings: &Settings) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut option = |text: String| {
        args.push("-o".to_owned());
        args.push(text);
    };
    option("BatchMode=yes".to_owned());
    option(format!(
        "ConnectTimeout={}",
        settings.connect_timeout.as_secs().max(1)
    ));
    if let Some(keepalive) = settings.keepalive {
        option(format!(
            "ServerAliveInterval={}",
            keepalive.as_secs().max(1)
        ));
        option(format!("ServerAliveCountMax={}", settings.keepalive_max));
    }
    match settings.check {
        HostKeyCheck::Strict => option("StrictHostKeyChecking=yes".to_owned()),
        HostKeyCheck::Tofu => option("StrictHostKeyChecking=accept-new".to_owned()),
        HostKeyCheck::Off => {
            option("StrictHostKeyChecking=no".to_owned());
            option(format!("UserKnownHostsFile={NO_KNOWN_HOSTS}"));
        }
    }
    if settings.check != HostKeyCheck::Off && settings.known_hosts_set {
        option(format!(
            "UserKnownHostsFile={}",
            settings.known_hosts.display()
        ));
    }
    if !settings.agent {
        option("IdentityAgent=none".to_owned());
    }
    if let Some(identity) = &settings.identity {
        args.push("-i".to_owned());
        args.push(identity.to_string_lossy().into_owned());
        args.push("-o".to_owned());
        args.push("IdentitiesOnly=yes".to_owned());
    }
    args.push("-p".to_owned());
    args.push(settings.port.to_string());
    args.push("-l".to_owned());
    args.push(settings.user.clone());
    args
}

/// The process that runs `ssh`, with its options and then `before` (flags such as `-tt` or `-s`), the host and `after` (the
/// command, or the subsystem).
pub(crate) fn command(
    link: &Link,
    before: &[&str],
    after: &[&str],
) -> Result<Command, ProviderError> {
    let settings = &link.settings;
    let Some(ssh) = &settings.openssh else {
        return Err(refuse(
            Status::GUATIAO_ERR_INTERNAL,
            "the source does not use the system's ssh client",
        ));
    };
    let mut command = Command::new(&ssh.program);
    command.args(options(settings));
    command.args(&ssh.args);
    command.args(before);
    command.arg(&settings.host);
    command.args(after);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    Ok(command)
}

/// Whether the host can be reached now: runs `true` on it, for at most `limit`. One login at a time, and its answer is kept
/// for `probe_every`, so a caller that asks often (each `info` of a peer) does not log in each time.
pub(crate) async fn probe(link: &Link, limit: Duration) -> Result<(), String> {
    let mut last = link.probed.lock().await;
    if let Some((when, outcome)) = last.as_ref()
        && when.elapsed() < link.settings.probe_every
    {
        return outcome.clone();
    }
    let outcome = login(link, limit).await;
    *last = Some((Instant::now(), outcome.clone()));
    outcome
}

async fn login(link: &Link, limit: Duration) -> Result<(), String> {
    let mut command =
        command(link, &["-T"], &["true"]).map_err(|error| error.message().to_string())?;
    command.stdin(Stdio::null());
    let run = async {
        let output = command.output().await.map_err(|error| {
            format!("{}: the ssh program could not run: {error}", link.target())
        })?;
        if output.status.success() {
            Ok(())
        } else {
            let text = String::from_utf8_lossy(&output.stderr);
            let line = text.lines().rev().find(|line| !line.trim().is_empty());
            Err(format!(
                "{}: {}",
                link.target(),
                line.unwrap_or("ssh did not connect")
            ))
        }
    };
    tokio::time::timeout(limit, run)
        .await
        .unwrap_or_else(|_| Err(format!("{}: no answer in time", link.target())))
}
