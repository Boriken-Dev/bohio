//! Named starting points for a host's transfer settings.
//!
//! A profile is a table of defaults laid under the configuration files: a key a file sets wins, key by key, and a
//! key neither sets is the built-in default. A profile fixes no key and sets no `[peer.*]` table. The names are
//! [`Profile`]'s; a client has the same names for what it asks (`bohio_client::profile`), and a client's profile and a
//! host's need not match.

#![forbid(unsafe_code)]

use bohio_client::profile::Profile;

use crate::config::NodeConfig;

/// The defaults `profile` lays under the configuration, as TOML.
///
/// Empty for `default`, which is the built-in behaviour.
#[must_use]
pub const fn table(profile: Profile) -> &'static str {
    match profile {
        Profile::Lan => LAN,
        Profile::WeakHost => WEAK_HOST,
        Profile::SlowLink => SLOW_LINK,
        Profile::FatLink => FAT_LINK,
        _ => "",
    }
}

/// A local network: larger frames, and a larger window for a bulk stream so one file is not limited by the
/// 1 MiB window while a round trip takes a few milliseconds.
const LAN: &str = r"
[filesystem]
chunk_size = 262144

[link]
bulk_window = 4194304
max_window = 8388608
";

/// A host that cannot compress or hash much: the cheap compressor, low caps, one hash and one walk at a time, a small
/// window that does not grow, and a small signature.
const WEAK_HOST: &str = r#"
[compression]
allowed = ["lz4", "zstd"]
default = { algorithm = "lz4" }
max_level = 3
window_log_max = 20
workers = 0

[hash]
max_streams = 1

[walk]
max_streams = 1

[filesystem]
max_transfers = 4

[link]
max_window = 1048576
auto_window = false

[delta]
max_signature_bytes = 8388608
"#;

/// A slow or distant path: the best ratio the host can make, long windows that find repeats further back, a window
/// that grows to fill the path, and room for the many small opens a sync makes.
const SLOW_LINK: &str = r#"
[compression]
default = { algorithm = "zstd", level = 19 }
window_log_max = 27
prepare_max_bytes = 4294967296

[link]
bulk_window = 4194304
max_window = 16777216
auto_window = true
max_opens_per_minute = 6000

[filesystem]
resume_ttl = "1h"
"#;

/// A long path with room in it (a round trip of a few hundred milliseconds and tens of megabits or more): one bulk
/// stream moves at most its receive window per round trip, so the window is raised to cover the path's
/// bandwidth-delay product at about 200 Mbit/s and 270 ms, and nothing is compressed unless asked.
const FAT_LINK: &str = r"
[link]
bulk_window = 8388608
";

impl NodeConfig {
    /// The settings of a node on `profile`: the built-in defaults with the profile's table laid over them. An
    /// embedding application starts from this and sets what it needs.
    #[must_use]
    pub fn with_profile(profile: Profile) -> Self {
        let text = table(profile);
        if text.is_empty() {
            return Self::default();
        }
        // The tables are this file's constants and each is read in a test: a failure here is a defect in them.
        toml::from_str(text).unwrap_or_default()
    }
}

impl crate::config::Config {
    /// Every setting a profile or a transfer depends on, with its effective value as text, in the order
    /// `bohio config check` prints them. A key is a dotted path into the file (`link.bulk_window`).
    #[must_use]
    pub fn transfer_keys(&self) -> Vec<(&'static str, String)> {
        fn show<T: core::fmt::Debug>(value: &T) -> String {
            format!("{value:?}")
        }
        fn maybe<T: core::fmt::Debug>(value: &Option<T>) -> String {
            value.as_ref().map_or_else(|| "unset".to_owned(), show)
        }
        fn names<T>(list: &[T], name: impl Fn(&T) -> String) -> String {
            let words: Vec<String> = list.iter().map(name).collect();
            format!("[{}]", words.join(", "))
        }
        let compression = &self.compression;
        let level = compression
            .default
            .level
            .map_or_else(String::new, |level| format!(":{level}"));
        let window = compression
            .default
            .window_log
            .map_or_else(String::new, |log| format!(" window_log {log}"));
        vec![
            ("filesystem.chunk_size", show(&self.filesystem.chunk_size)),
            (
                "filesystem.max_transfers",
                show(&self.filesystem.max_transfers),
            ),
            (
                "filesystem.idle_timeout",
                show(&self.filesystem.idle_timeout),
            ),
            ("filesystem.resume_ttl", show(&self.filesystem.resume_ttl)),
            ("link.window", show(&self.link.window)),
            ("link.bulk_window", maybe(&self.link.bulk_window)),
            ("link.max_window", maybe(&self.link.max_window)),
            ("link.auto_window", show(&self.link.auto_window)),
            ("link.max_streams", show(&self.link.max_streams)),
            (
                "link.max_opens_per_minute",
                show(&self.link.max_opens_per_minute),
            ),
            ("link.max_frame", show(&self.link.max_frame)),
            (
                "compression.allowed",
                names(&compression.allowed, |codec| codec.name().to_owned()),
            ),
            (
                "compression.default",
                format!("{}{level}{window}", compression.default.algorithm.name()),
            ),
            ("compression.max_level", show(&compression.max_level)),
            (
                "compression.window_log_max",
                show(&compression.window_log_max),
            ),
            ("compression.workers", show(&compression.workers)),
            (
                "compression.prepare_max_bytes",
                show(&compression.prepare_max_bytes),
            ),
            ("hash.max_streams", show(&self.hash.max_streams)),
            ("walk.max_streams", show(&self.walk.max_streams)),
            (
                "delta.max_signature_bytes",
                show(&self.delta.max_signature_bytes),
            ),
            (
                "client.parallel_streams",
                show(&self.client.parallel_streams),
            ),
            ("client.parallel_links", show(&self.client.parallel_links)),
            ("client.profile", maybe(&self.client.profile)),
        ]
    }
}
