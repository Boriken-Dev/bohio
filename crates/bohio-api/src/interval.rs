// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! [`Interval`]: a length of time in a provider's settings, written as one syntax everywhere.

use std::time::Duration;

use guatiao::library::ProviderError;
use guatiao::schema::build::KindBuilder;
use guatiao::{
    Alloc, FromValue, Kinds, MapError, Number, Schema, Status, Tag, Text, ToValue, TryAsRef, Value,
    ValueError,
};

/// What a refused interval was expected to be.
const EXPECTED: &str =
    "a time: a number of seconds, or a number and ms, s, m or h (`500ms`, `30s`)";

/// A length of time as a provider's setting reads it: a number of seconds (`5`), or a number and a unit (`500ms`,
/// `30s`, `5m`, `1h`). `bohio.cfg`'s own times read the same.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, PartialOrd, Ord, Hash)]
pub struct Interval(pub Duration);

impl Interval {
    /// `secs` whole seconds.
    #[must_use]
    pub const fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }

    /// `millis` milliseconds.
    #[must_use]
    pub const fn from_millis(millis: u64) -> Self {
        Self(Duration::from_millis(millis))
    }

    /// The length as a [`Duration`].
    #[must_use]
    pub const fn get(self) -> Duration {
        self.0
    }

    /// Reads `text`: a number of seconds, or a number and `ms`, `s`, `m` or `h`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let digits = text.chars().take_while(char::is_ascii_digit).count();
        let (number, unit) = text.split_at(digits);
        let number: u64 = number.parse().ok()?;
        match unit.trim() {
            "ms" => Some(Self::from_millis(number)),
            "" | "s" => Some(Self::from_secs(number)),
            "m" => Some(Self::from_secs(number.checked_mul(60)?)),
            "h" => Some(Self::from_secs(number.checked_mul(3600)?)),
            _ => None,
        }
    }

    /// The length as text that [`Interval::parse`] reads back.
    #[must_use]
    pub fn text(self) -> String {
        if self.0.subsec_nanos() == 0 {
            format!("{}s", self.0.as_secs())
        } else {
            format!("{}ms", self.0.as_millis())
        }
    }

    /// An error for a setting `key` that is a zero length where one above zero is needed.
    ///
    /// # Errors
    /// Always: the status is a bad value.
    pub fn zero_refused(key: &str) -> ProviderError {
        ProviderError::new(
            Status::GUATIAO_ERR_BAD_VALUE,
            &format!("{key} is 0, which could never succeed (a longer time)"),
        )
    }
}

impl From<Duration> for Interval {
    fn from(duration: Duration) -> Self {
        Self(duration)
    }
}

impl ToValue for Interval {
    fn to_value_in(&self, alloc: Alloc) -> Result<Value, ValueError> {
        Ok(Text::new_in(alloc, &self.text())?.into())
    }
}

/// A number of seconds or text with a unit.
impl FromValue for Interval {
    const KINDS: Kinds = Kinds::NUMBER.union(Kinds::STRING);

    fn from_value(value: &Value) -> Result<Self, MapError> {
        let refused = || MapError::bad_value(EXPECTED);
        let text: &str = match value.tag() {
            Ok(Tag::GUATIAO_NUMBER) => TryAsRef::<Number>::try_as_ref(value)
                .ok_or_else(refused)?
                .as_ref(),
            _ => <&str>::try_from(value)?,
        };
        Self::parse(text).ok_or_else(refused)
    }
}

/// A string, since a number is the same time written without its unit.
impl Schema for Interval {
    fn kind(alloc: Alloc) -> KindBuilder {
        KindBuilder::string_in(alloc).description(EXPECTED)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_is_seconds_or_a_number_and_a_unit() {
        assert_eq!(Interval::parse("15s"), Some(Interval::from_secs(15)));
        assert_eq!(Interval::parse("250ms"), Some(Interval::from_millis(250)));
        assert_eq!(Interval::parse("2"), Some(Interval::from_secs(2)));
        assert_eq!(Interval::parse("1h"), Some(Interval::from_secs(3600)));
        assert_eq!(Interval::parse("5 m"), Some(Interval::from_secs(300)));
        assert_eq!(Interval::parse("soon"), None);
        assert_eq!(Interval::parse("-1s"), None);
        assert_eq!(Interval::parse("1d"), None);
    }

    #[test]
    fn a_time_written_out_reads_back_as_itself() {
        for time in [
            Interval::from_secs(0),
            Interval::from_secs(30),
            Interval::from_millis(1500),
        ] {
            assert_eq!(Interval::parse(&time.text()), Some(time));
        }
    }

    #[test]
    fn a_setting_takes_a_bare_number_and_text_alike() {
        let number = 5_u64.to_value().unwrap();
        let text = "5s".to_value().unwrap();
        assert_eq!(
            Interval::from_value(&number).unwrap(),
            Interval::from_secs(5)
        );
        assert_eq!(Interval::from_value(&text).unwrap(), Interval::from_secs(5));
        assert!(Interval::from_value(&"soon".to_value().unwrap()).is_err());
        assert!(Interval::from_value(&true.to_value().unwrap()).is_err());
    }
}
