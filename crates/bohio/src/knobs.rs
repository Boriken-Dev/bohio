// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The settings types the native sources and the platform layer read, apart from the node's configuration so a build
//! without feature `node` has them. `config` re-exports each.

#![forbid(unsafe_code)]

use std::fmt;
use std::time::Duration;

use serde::Deserialize;
use serde::de::{self, Deserializer};

/// `[execution] host_death`: what a host does about its processes when it dies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HostDeath {
    /// End each process group with SIGKILL.
    #[default]
    Kill,
    /// Send each process group SIGTERM; a program that handles or ignores it lives on.
    Term,
    /// Leave the processes running: a killed host leaves its trees behind.
    Leave,
}

impl HostDeath {
    /// The word `[shell] host_death` and a terminal request use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Kill => "kill",
            Self::Term => "term",
            Self::Leave => "leave",
        }
    }
}

/// A length of time, written as a number of seconds or as text with a unit.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Span(pub Duration);

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl Span {
    /// Reads `30`, `250ms`, `5s`, `2m` or `1h` (a bare number is seconds).
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let split = text
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(text.len());
        let (digits, unit) = text.split_at(split);
        let count: u64 = digits.parse().ok()?;
        let duration = match unit.trim() {
            "" | "s" => Duration::from_secs(count),
            "ms" => Duration::from_millis(count),
            "m" => Duration::from_secs(count.checked_mul(60)?),
            "h" => Duration::from_secs(count.checked_mul(3600)?),
            _ => return None,
        };
        Some(Self(duration))
    }
}

impl<'de> Deserialize<'de> for Span {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl de::Visitor<'_> for Visitor {
            type Value = Span;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a number of seconds or text such as `250ms`, `5s`, `2m`, `1h`")
            }
            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Span, E> {
                Ok(Span(Duration::from_secs(value)))
            }
            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Span, E> {
                u64::try_from(value)
                    .map(|v| Span(Duration::from_secs(v)))
                    .map_err(|_| E::custom("a time cannot be negative"))
            }
            fn visit_str<E: de::Error>(self, value: &str) -> Result<Span, E> {
                Span::parse(value).ok_or_else(|| {
                    E::custom("not a time: use a number of seconds or a unit of ms, s, m or h")
                })
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// How the Windows backend captures the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowsCapture {
    /// DXGI Desktop Duplication, and GDI for a display it cannot duplicate (a rotated one, a remote session, an adapter
    /// without it).
    Auto,
    /// DXGI Desktop Duplication only: a display it cannot duplicate is `unavailable`.
    Dxgi,
    /// GDI only: slower on a large screen, and it works wherever a screen is drawn.
    Gdi,
}
