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

/// Whether an engine over the operating system is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Switch {
    /// Served, as `[filesystem]` and `[execution]` say.
    #[default]
    Native,
    /// Not served: every call is `unsupported`, and `info.access` says so.
    None,
}

impl Switch {
    /// The word `info.access` reports.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::None => "none",
        }
    }
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
    /// Whether files are served (the engine over the operating system), `native` or `none`. Default `native`.
    pub fs: Switch,
    /// Whether processes are run (the engine over the operating system), `native` or `none`. Default `native`.
    pub exec: Switch,
}

/// The `[source.*]` tables of a configuration.
pub type SourceTables = BTreeMap<String, SourceTable>;

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
