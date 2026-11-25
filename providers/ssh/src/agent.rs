// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Logging in with a key an ssh agent holds: the OpenSSH agent's named pipe or Pageant on Windows, `SSH_AUTH_SOCK`
//! elsewhere. The agent signs; the key never reaches this process.

use russh::client::Handle;
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::{Algorithm, HashAlg};

use crate::link::Client;

/// Tries every key the agent holds; whether one was accepted.
async fn offer<S: AgentStream + Unpin + Send + 'static>(
    mut agent: AgentClient<S>,
    handle: &mut Handle<Client>,
    user: &str,
    rsa_hash: Option<HashAlg>,
) -> Result<bool, String> {
    let identities = agent
        .request_identities()
        .await
        .map_err(|error| error.to_string())?;
    for identity in identities {
        let AgentIdentity::PublicKey { key, .. } = identity else {
            continue;
        };
        let hash = matches!(key.algorithm(), Algorithm::Rsa { .. })
            .then_some(rsa_hash)
            .flatten();
        // A key the agent declines to sign with, or the host refuses, leaves the next one to try.
        if let Ok(result) = handle
            .authenticate_publickey_with(user, key, hash, &mut agent)
            .await
            && result.success()
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(unix)]
pub(crate) async fn login(
    handle: &mut Handle<Client>,
    user: &str,
    rsa_hash: Option<HashAlg>,
) -> Result<bool, String> {
    let agent = AgentClient::connect_env()
        .await
        .map_err(|error| error.to_string())?;
    offer(agent, handle, user, rsa_hash).await
}

/// The named pipe of Windows' OpenSSH agent.
#[cfg(windows)]
const OPENSSH_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

#[cfg(windows)]
pub(crate) async fn login(
    handle: &mut Handle<Client>,
    user: &str,
    rsa_hash: Option<HashAlg>,
) -> Result<bool, String> {
    let named = std::env::var("SSH_AUTH_SOCK")
        .ok()
        .filter(|path| path.starts_with(r"\\.\pipe\"));
    let pipe = named.as_deref().unwrap_or(OPENSSH_PIPE);
    let mut why = Vec::new();
    match AgentClient::connect_named_pipe(pipe).await {
        Ok(agent) => return offer(agent, handle, user, rsa_hash).await,
        Err(error) => why.push(format!("{pipe}: {error}")),
    }
    match AgentClient::connect_pageant().await {
        Ok(agent) => return offer(agent, handle, user, rsa_hash).await,
        Err(error) => why.push(format!("pageant: {error}")),
    }
    Err(why.join("; "))
}

#[cfg(not(any(unix, windows)))]
pub(crate) async fn login(
    _handle: &mut Handle<Client>,
    _user: &str,
    _rsa_hash: Option<HashAlg>,
) -> Result<bool, String> {
    Err("this platform has no agent".to_owned())
}
