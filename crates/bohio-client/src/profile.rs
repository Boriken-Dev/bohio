//! Named starting points for what a client asks of a host.
//!
//! A [`Profile`] is a name for a kind of path or machine; [`Profile::defaults`] says what a client on it asks for when
//! the caller names nothing. Every value is a default for something the caller can still set: a profile never limits
//! what a person may ask. The host has the same names (`[profile] name` in its `bohio.cfg`) and chooses its own caps
//! separately, so a client's profile and a host's profile need not match.

use bohio_proto::compress::{Codec, Compression};
use bohio_proto::hash::Algorithm;

use crate::sync::{Compare, DeltaMode};

/// A kind of path or machine, by the name a configuration gives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Profile {
    /// Today's behaviour: nothing is compressed unless asked, one stream, the link's own windows.
    #[default]
    Default,
    /// A local network: no compression, two streams, a larger receive window for bulk streams.
    Lan,
    /// A weak machine: the cheap compression, the cheap digest, one stream.
    WeakHost,
    /// A slow or distant path: squeeze before sending, compare by digest, several streams.
    SlowLink,
    /// A long path with room in it: one bulk stream is limited by its window, so the window is larger.
    FatLink,
}

impl Profile {
    /// Every profile, in the order a listing shows them.
    pub const ALL: [Self; 5] = [
        Self::Default,
        Self::Lan,
        Self::WeakHost,
        Self::SlowLink,
        Self::FatLink,
    ];

    /// The name a configuration and a flag use: `default`, `lan`, `weak-host`, `slow-link` or `fat-link`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Lan => "lan",
            Self::WeakHost => "weak-host",
            Self::SlowLink => "slow-link",
            Self::FatLink => "fat-link",
        }
    }

    /// The profile named `name`, or `None` for a name this version does not have.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|profile| profile.name() == name)
    }

    /// The names of every profile, for a message that says which are known.
    #[must_use]
    pub fn names() -> String {
        Self::ALL.map(Self::name).join(", ")
    }

    /// What a client on this profile asks for where the caller names nothing.
    #[must_use]
    pub fn defaults(self) -> ClientDefaults {
        let mut defaults = ClientDefaults::default();
        match self {
            Self::Default => {}
            Self::Lan => {
                defaults.parallel = 2;
                defaults.bulk_window = Some(4 * MIB);
            }
            Self::WeakHost => {
                defaults.compression = Some(Compression::new(Codec::Lz4));
                defaults.compare = Some(Compare::Hash(Algorithm::Xxh128));
            }
            Self::SlowLink => {
                defaults.compression = Some(
                    Compression::new(Codec::Zstd)
                        .level(19)
                        .window_log(crate::fs::DEFAULT_WINDOW_LOG_MAX),
                );
                defaults.compare = Some(Compare::Hash(Algorithm::Blake3));
                defaults.parallel = 4;
                defaults.bulk_window = Some(4 * MIB);
                defaults.prepare_min = Some(16 * MIB as u64);
            }
            Self::FatLink => {
                defaults.bulk_window = Some(8 * MIB);
            }
        }
        defaults
    }
}

const MIB: u32 = 1024 * 1024;

/// What a [`Profile`] asks of a host for a caller that named nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ClientDefaults {
    /// How to compress what travels; `None` asks for none.
    pub compression: Option<Compression>,
    /// Whether a transfer uses a delta against the copy at the destination.
    pub delta: DeltaMode,
    /// How far to compare before a file is moved; `None` compares nothing.
    pub compare: Option<Compare>,
    /// How many streams carry one file at once (at least 1).
    pub parallel: usize,
    /// The receive window, in bytes, this side grants a bulk stream it receives; `None` is the link's `window`.
    pub bulk_window: Option<u32>,
    /// The size of file, in bytes, from which a compressed read is prepared in full first; `None` never prepares.
    pub prepare_min: Option<u64>,
}

impl Default for ClientDefaults {
    fn default() -> Self {
        Self {
            compression: None,
            delta: DeltaMode::Auto,
            compare: None,
            parallel: 1,
            bulk_window: None,
            prepare_min: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_name_parses_back_and_another_does_not() {
        for profile in Profile::ALL {
            assert_eq!(Profile::parse(profile.name()), Some(profile));
        }
        assert_eq!(Profile::parse("turbo"), None);
        assert_eq!(Profile::parse("Lan"), None);
        assert_eq!(Profile::default(), Profile::Default);
    }

    #[test]
    fn what_each_profile_asks_for() {
        let default = Profile::Default.defaults();
        assert_eq!(default, ClientDefaults::default());
        assert_eq!(default.compression, None);
        assert_eq!(default.parallel, 1);

        let lan = Profile::Lan.defaults();
        assert_eq!((lan.compression, lan.parallel), (None, 2));

        let weak = Profile::WeakHost.defaults();
        assert_eq!(weak.compression, Some(Compression::new(Codec::Lz4)));
        assert_eq!(weak.compare, Some(Compare::Hash(Algorithm::Xxh128)));
        assert_eq!(weak.parallel, 1);

        let slow = Profile::SlowLink.defaults();
        assert_eq!(
            slow.compression,
            Some(
                Compression::new(Codec::Zstd)
                    .level(19)
                    .window_log(crate::fs::DEFAULT_WINDOW_LOG_MAX)
            )
        );
        assert_eq!(slow.compare, Some(Compare::Hash(Algorithm::Blake3)));
        assert_eq!(slow.delta, DeltaMode::Auto);
        assert_eq!(slow.parallel, 4);
        assert_eq!(slow.prepare_min, Some(16 * 1024 * 1024));

        let fat = Profile::FatLink.defaults();
        assert_eq!(fat.bulk_window, Some(8 * 1024 * 1024));
        assert_eq!(fat.compression, None);
    }

    #[test]
    fn a_profile_never_asks_for_less_than_one_stream() {
        for profile in Profile::ALL {
            assert!(profile.defaults().parallel >= 1, "{}", profile.name());
        }
    }
}
