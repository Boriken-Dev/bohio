//! `[source.<name>]` and `[access]`: how a node reaches the system it is for.
//!
//! A **source** is a configured instance of a provider; `[access]` says which sources serve each capability, the first
//! being the default. A node with neither table is today's node: the `native` source for terminal, desktop, files and
//! processes.

use std::collections::BTreeMap;

use serde::Deserialize;

/// The source every node has: its own pseudo-terminal and screen. It is a provider like any other.
pub const NATIVE: &str = "native";

/// One `[source.<name>]` table: the provider and that provider's own settings.
#[derive(Debug, Clone, PartialEq, Deserialize, Default)]
pub struct SourceTable {
    /// The provider's id. `native` needs none (it is `native`); every other source names one.
    #[serde(default)]
    pub provider: Option<String>,
    /// The provider's own settings, checked against its schema before it is built.
    #[serde(flatten)]
    pub settings: toml::Table,
}

/// `[access]`: which sources serve each capability. A list names sources of `[source.<name>]` (or `native`); the first
/// is the default, and a peer's `sources` may name the others.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AccessConfig {
    /// The terminal sources. Absent: `["native"]`. `[]`: the node serves no shell.
    pub terminal: Option<Vec<String>>,
    /// The desktop sources. Absent: `["native"]`. `[]`: the node serves no desktop.
    pub desktop: Option<Vec<String>>,
    /// The power sources. Default none.
    pub power: Vec<String>,
    /// The virtual-media sources. Default none.
    pub media: Vec<String>,
    /// The files sources. Absent or `"native"`: `["native"]`. `"none"` or `[]`: the node serves no files.
    #[serde(deserialize_with = "sources_or_word")]
    pub fs: Option<Vec<String>>,
    /// The exec sources. Absent or `"native"`: `["native"]`. `"none"` or `[]`: the node runs no processes.
    #[serde(deserialize_with = "sources_or_word")]
    pub exec: Option<Vec<String>>,
}

/// The `[source.*]` tables of a configuration.
pub type SourceTables = BTreeMap<String, SourceTable>;

/// A list of source names, or the word `native` (the node's own source alone) or `none` (no source).
fn sources_or_word<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Form {
        Word(String),
        List(Vec<String>),
    }
    Ok(match Form::deserialize(deserializer)? {
        Form::Word(word) if word == NATIVE => None,
        Form::Word(word) if word == "none" => Some(Vec::new()),
        Form::Word(word) => {
            return Err(serde::de::Error::custom(format!(
                "`{word}` is not `native`, `none` or a list of source names"
            )));
        }
        Form::List(list) => Some(list),
    })
}

impl AccessConfig {
    /// The sources of the terminal capability, the default applied.
    #[must_use]
    pub fn terminals(&self) -> Vec<String> {
        self.terminal
            .clone()
            .unwrap_or_else(|| vec![NATIVE.to_owned()])
    }

    /// The sources of the desktop capability, the default applied.
    #[must_use]
    pub fn desktops(&self) -> Vec<String> {
        self.desktop
            .clone()
            .unwrap_or_else(|| vec![NATIVE.to_owned()])
    }
}

impl AccessConfig {
    /// The sources of the exec capability, the default applied.
    #[must_use]
    pub fn execs(&self) -> Vec<String> {
        self.exec.clone().unwrap_or_else(|| vec![NATIVE.to_owned()])
    }

    /// The sources of the files capability, the default applied.
    #[must_use]
    pub fn filesystems(&self) -> Vec<String> {
        self.fs.clone().unwrap_or_else(|| vec![NATIVE.to_owned()])
    }
}
