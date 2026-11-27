//! What reads, writes and exec streams share about compression: the host's settings applied to a
//! request, the files it never compresses, and the decoder a stream's peer-chosen bytes go through.

use std::io::Write;

use bohio_proto::compress::{Codec, Compression};
use bohio_proto::{ErrorBody, ErrorCode};
use bohio_transfer::compress::{Bounds, Caps, DecodeError, Decoder, Unsupported};
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::config::CompressionConfig;

use super::io_error;

/// The `[compression]` settings, ready to apply to requests.
pub(crate) struct Settings<'a> {
    config: &'a CompressionConfig,
    caps: Caps,
}

impl<'a> Settings<'a> {
    pub(crate) fn new(config: &'a CompressionConfig) -> Self {
        Self {
            caps: config.caps(),
            config,
        }
    }

    pub(crate) const fn caps(&self) -> &Caps {
        &self.caps
    }

    pub(crate) const fn config(&self) -> &'a CompressionConfig {
        self.config
    }

    /// What a request becomes: `auto` is the host's default, a level is clamped, and an algorithm the
    /// host does not allow is `unsupported`. `None` for a request that is not compressed (absent or
    /// `none`).
    ///
    /// # Errors
    ///
    /// `unsupported` for an algorithm not allowed, not built or not known.
    pub(crate) fn resolve(
        &self,
        request: Option<&Compression>,
    ) -> Result<Option<Compression>, ErrorBody> {
        let Some(request) = request else {
            return Ok(None);
        };
        let wanted = match &request.algorithm {
            Codec::None => return Ok(None),
            Codec::Auto => Compression {
                algorithm: self.config.default.algorithm.clone(),
                level: request.level.or(self.config.default.level),
                window_log: request.window_log.or(self.config.default.window_log),
            },
            _ => request.clone(),
        };
        self.concrete(&wanted).map(Some)
    }

    /// As [`Settings::resolve`] for a direction the client sends: the client names the algorithm, so
    /// `auto` is a bad request.
    pub(crate) fn declared(
        &self,
        request: Option<&Compression>,
    ) -> Result<Option<Compression>, ErrorBody> {
        if request.is_some_and(|request| request.algorithm == Codec::Auto) {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "name an algorithm for what you send: auto is the host's choice for what it sends",
            ));
        }
        self.resolve(request)
    }

    fn concrete(&self, wanted: &Compression) -> Result<Compression, ErrorBody> {
        let refused = |name: &Codec| {
            ErrorBody::new(
                ErrorCode::Unsupported,
                format!("this host does not offer {name} compression"),
            )
        };
        if !self.config.allowed.contains(&wanted.algorithm) {
            return Err(refused(&wanted.algorithm));
        }
        self.caps
            .clamp(wanted)
            .map_err(|Unsupported(codec)| refused(&codec))
    }

    /// The bounds a decoder of what a peer sends runs under.
    pub(crate) fn bounds(&self) -> Bounds {
        Bounds {
            window_log_max: self.config.window_log_max,
            max_output: None,
            max_expansion: self.config.max_expansion,
        }
    }

    /// A decoder for what the client declared it sends.
    pub(crate) fn decoder(&self, used: &Compression) -> Result<Decoder, ErrorBody> {
        Decoder::new(used, &self.bounds())
            .map_err(|error| ErrorBody::new(ErrorCode::Unsupported, error.to_string()))
    }

    /// As [`Settings::skips`], for a path that may be a link: it is also never compressed when what a link on the
    /// way names is, so `notes.txt -> id_rsa` is held to the rule `id_rsa` is.
    pub(crate) fn skips_through(&self, roots: &super::ops::Roots<'_>, path: &str) -> bool {
        self.skips(path)
            || roots
                .link_names(path)
                .iter()
                .any(|reached| self.skips(reached))
    }

    /// Whether a file is never compressed: its extension is on `skip_extensions`, or its path or name
    /// matches an `exclude` glob.
    pub(crate) fn skips(&self, path: &str) -> bool {
        let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
        if let Some((_, extension)) = name.rsplit_once('.')
            && self.config.skip_extensions.iter().any(|skipped| {
                skipped
                    .trim_start_matches('.')
                    .eq_ignore_ascii_case(extension)
            })
        {
            return true;
        }
        let mut builder = GlobSetBuilder::new();
        for pattern in &self.config.exclude {
            // A pattern that does not parse was refused when the settings were checked.
            // Names compare without case where the file system does not tell `Key` from `key`.
            if let Ok(glob) = GlobBuilder::new(pattern)
                .case_insensitive(cfg!(windows))
                .build()
            {
                builder.add(glob);
            }
        }
        let set: GlobSet = builder.build().unwrap_or_else(|_| GlobSet::empty());
        set.is_match(path) || set.is_match(name)
    }
}

/// Where the bytes a peer sent compressed go once decoded: the file, the digests and a count.
pub(crate) struct Sink<'a, W: Write> {
    pub(crate) out: &'a mut W,
    pub(crate) hasher: &'a mut bohio_transfer::hash::Hasher,
    pub(crate) written: &'a mut u64,
}

impl<W: Write> Write for Sink<'_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.out.write_all(bytes)?;
        self.hasher.update(bytes);
        *self.written += bytes.len() as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.out.flush()
    }
}

/// A writer that keeps the first error of what it writes to and takes every byte after it, so that a decoder runs to the
/// end of a piece (its context spans the pieces) while the caller learns that the destination failed.
pub(crate) struct Tolerant<W: Write> {
    pub(crate) inner: W,
    /// The first error `inner` gave; nothing is written to it after.
    pub(crate) failed: Option<std::io::Error>,
}

impl<W: Write> Tolerant<W> {
    pub(crate) const fn new(inner: W) -> Self {
        Self {
            inner,
            failed: None,
        }
    }
}

impl<W: Write> Write for Tolerant<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.failed.is_none()
            && let Err(error) = self.inner.write_all(bytes)
        {
            self.failed = Some(error);
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The error a peer gets for compressed data that cannot be taken.
pub(crate) fn decode_failure(error: &DecodeError) -> ErrorBody {
    match error {
        DecodeError::Write(kind) => io_error(&std::io::Error::from(*kind)),
        DecodeError::Expansion => ErrorBody::new(
            ErrorCode::BadRequest,
            "the data expands further than this host allows",
        ),
        DecodeError::Unsupported(_) | DecodeError::Setup(_) => {
            ErrorBody::new(ErrorCode::Unsupported, error.to_string())
        }
        _ => ErrorBody::new(ErrorCode::BadRequest, error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A destination that takes `room` bytes and then fails, as a full disk does.
    struct Full {
        room: usize,
        taken: usize,
    }

    impl Write for Full {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self.taken + bytes.len() > self.room {
                return Err(std::io::Error::from(std::io::ErrorKind::StorageFull));
            }
            self.taken += bytes.len();
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_failing_destination_is_told_once_and_the_writer_takes_what_follows() {
        let mut full = Full { room: 4, taken: 0 };
        let mut tolerant = Tolerant::new(&mut full);
        assert_eq!(tolerant.write(b"abc").unwrap(), 3);
        assert!(tolerant.failed.is_none());
        // The write that does not fit fails the destination; the decoder is still handed every byte.
        assert_eq!(tolerant.write(b"defg").unwrap(), 4);
        let first = tolerant.failed.as_ref().map(std::io::Error::kind);
        assert_eq!(first, Some(std::io::ErrorKind::StorageFull));
        assert_eq!(tolerant.write(b"h").unwrap(), 1);
        assert_eq!(
            tolerant.failed.as_ref().map(std::io::Error::kind),
            first,
            "the first error is kept"
        );
        assert_eq!(full.taken, 3, "nothing is written after the failure");
    }
}
