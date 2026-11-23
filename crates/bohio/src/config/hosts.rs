//! `[remote.<name>]`: hosts a person or an agent names, so `bohio remote NAME ...` and a jump list can say the name.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

/// One named host: what `bohio remote` takes on its command line to reach it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HostConfig {
    /// Where the host is: any endpoint `bohio remote` takes (`tcp://`, `ws://`, `wss://`, `webtransport://`, `pipe://`,
    /// `serial://`). Required: a table without one is refused by `bohio config check`, naming the key.
    pub endpoint: String,
    /// The id the host must prove (`--server`). Absent: the host is trusted by the keys this node holds, as with no
    /// `--server`.
    pub server: Option<String>,
    /// Certificate authority files the TLS endpoint is checked against (`--ca`).
    pub ca: Vec<PathBuf>,
    /// SHA-256 hashes of certificates the TLS endpoint may present (`--cert-hash`).
    pub cert_hash: Vec<String>,
    /// An endpoint to try when this one does not answer (`--fallback`).
    pub fallback: Option<String>,
    /// The hosts to go through, in order, as `--jump` does: each entry is a name of this table or an endpoint. A named
    /// jump brings its own `server`, `ca`, `cert_hash` and `fallback`; an endpoint is trusted by the keys this node
    /// holds.
    pub jump: Vec<String>,
}

/// The named hosts of a configuration, by name.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(transparent)]
pub struct Hosts(BTreeMap<String, HostConfig>);

impl Hosts {
    /// The host `name` names.
    pub fn get(&self, name: &str) -> Option<&HostConfig> {
        self.0.get(name)
    }

    /// The names, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(String::as_str)
    }

    /// Every host with its name, in order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &HostConfig)> {
        self.0.iter().map(|(name, host)| (name.as_str(), host))
    }

    /// How many hosts are named.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether none is.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether `text` names a host and is not an endpoint: a word without `://`.
    pub fn is_name(text: &str) -> bool {
        !text.contains("://")
    }

    /// The first problem: the key and what is wrong. An empty `endpoint`, a name that is not a word, a `jump` entry that
    /// names no host, and a host that is its own jump through any chain of names.
    ///
    /// # Errors
    ///
    /// `(key, message)` of the first problem found, the key as written in the file (`remote.web.jump`).
    pub fn check(&self) -> Result<(), (String, String)> {
        for (name, host) in &self.0 {
            if name.is_empty() || name.contains("://") {
                return Err((
                    format!("remote.{name}"),
                    "a host name is a word, not an endpoint".to_owned(),
                ));
            }
            if host.endpoint.trim().is_empty() {
                return Err((
                    format!("remote.{name}.endpoint"),
                    "is required: the endpoint the name stands for".to_owned(),
                ));
            }
            for entry in &host.jump {
                if Self::is_name(entry) && !self.0.contains_key(entry) {
                    return Err((
                        format!("remote.{name}.jump"),
                        format!(
                            "`{entry}` is not a host named in [remote] (an endpoint has `://`)"
                        ),
                    ));
                }
            }
        }
        for name in self.0.keys() {
            if let Some(cycle) = self.cycle_from(name) {
                return Err((
                    format!("remote.{name}.jump"),
                    format!("goes through itself: {cycle}"),
                ));
            }
        }
        Ok(())
    }

    /// The chain of names that leads from `start` back to it through `jump`, as `a -> b -> a`.
    fn cycle_from(&self, start: &str) -> Option<String> {
        let mut path = vec![start.to_owned()];
        self.walk(start, start, &mut path)
    }

    fn walk(&self, start: &str, at: &str, path: &mut Vec<String>) -> Option<String> {
        let host = self.0.get(at)?;
        for entry in &host.jump {
            if !Self::is_name(entry) {
                continue;
            }
            if entry == start {
                return Some(format!("{} -> {start}", path.join(" -> ")));
            }
            // A name seen before on this path is a cycle that does not include `start`: its own check reports it.
            if path.contains(entry) {
                continue;
            }
            path.push(entry.clone());
            if let Some(found) = self.walk(start, entry, path) {
                return Some(found);
            }
            path.pop();
        }
        None
    }

    /// The jump hops of `name` in order, each a [`Hop`], with a named hop's own jumps first: a jump that has jumps of its
    /// own is reached through them. The host itself is not in the list.
    ///
    /// # Errors
    ///
    /// The text of the first problem, for a name that is not in the table; [`Hosts::check`] has found none in a valid
    /// configuration.
    pub fn hops(&self, name: &str) -> Result<Vec<Hop>, String> {
        let host = self
            .0
            .get(name)
            .ok_or_else(|| format!("`{name}` is not a host named in [remote]"))?;
        let mut hops = Vec::new();
        for entry in &host.jump {
            if Self::is_name(entry) {
                hops.extend(self.hops(entry)?);
                let jump = self
                    .0
                    .get(entry)
                    .ok_or_else(|| format!("`{entry}` is not a host named in [remote]"))?;
                hops.push(Hop {
                    endpoint: jump.endpoint.clone(),
                    server: jump.server.clone(),
                    ca: jump.ca.clone(),
                    cert_hash: jump.cert_hash.clone(),
                });
            } else {
                hops.push(Hop {
                    endpoint: entry.clone(),
                    server: None,
                    ca: Vec::new(),
                    cert_hash: Vec::new(),
                });
            }
        }
        Ok(hops)
    }
}

/// One jump to go through: where it is, and the id it must prove when the configuration says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hop {
    /// The endpoint, as `--jump` takes it.
    pub endpoint: String,
    /// The id it must prove, as `--jump-server` takes it.
    pub server: Option<String>,
    /// Certificate authority files for the TLS endpoint, when this hop is the one dialed from here.
    pub ca: Vec<PathBuf>,
    /// Certificate hashes for the same.
    pub cert_hash: Vec<String>,
}
