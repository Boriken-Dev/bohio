// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! A link that names the files source of the host: every file stream and control call opened through it asks that source.

use bohio_proto::protocol;
use cabuya::mux::LinkStream;

use crate::{Error, Open, OpenOptions, connect};

/// `link`, with the files source `name` on every filesystem stream and every control call that opens through it. The
/// host's default source serves a request that names none.
pub struct Sourced<'a> {
    link: Held<'a>,
    name: String,
}

enum Held<'a> {
    Borrowed(&'a dyn Open),
    Owned(std::sync::Arc<dyn Open>),
}

impl Held<'_> {
    fn get(&self) -> &dyn Open {
        match self {
            Self::Borrowed(link) => *link,
            Self::Owned(link) => link.as_ref(),
        }
    }
}

impl core::fmt::Debug for dyn Open + '_ {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Open").finish_non_exhaustive()
    }
}

impl<'a> Sourced<'a> {
    /// `link` asking the files source `name`.
    #[must_use]
    pub fn new(link: &'a dyn Open, name: impl Into<String>) -> Self {
        Self {
            link: Held::Borrowed(link),
            name: name.into(),
        }
    }

    /// As [`Sourced::new`], for a link that is shared.
    #[must_use]
    pub fn shared(link: std::sync::Arc<dyn Open>, name: impl Into<String>) -> Sourced<'static> {
        Sourced {
            link: Held::Owned(link),
            name: name.into(),
        }
    }

    /// The metadata of a request for a filesystem stream, with this source named; any other stream's is as it was.
    fn named(&self, protocol: &str, metadata: &[u8]) -> Result<Vec<u8>, Error> {
        let source = Some(self.name.clone());
        let rewritten = match protocol {
            protocol::READ => {
                bohio_proto::fs::ReadRequest::from_metadata(metadata).and_then(|mut request| {
                    request.source = source;
                    request.to_metadata()
                })
            }
            protocol::WRITE => {
                bohio_proto::fs::WriteRequest::from_metadata(metadata).and_then(|mut request| {
                    request.source = source;
                    request.to_metadata()
                })
            }
            protocol::HASH => {
                bohio_proto::hash::HashRequest::from_metadata(metadata).and_then(|mut request| {
                    request.source = source;
                    request.to_metadata()
                })
            }
            protocol::WALK => {
                bohio_proto::walk::WalkRequest::from_metadata(metadata).and_then(|mut request| {
                    request.source = source;
                    request.to_metadata()
                })
            }
            protocol::SEARCH => bohio_proto::search::SearchRequest::from_metadata(metadata)
                .and_then(|mut request| {
                    request.source = source;
                    request.to_metadata()
                }),
            _ => return Ok(metadata.to_vec()),
        };
        rewritten.map_err(Error::Protocol)
    }
}

impl Open for Sourced<'_> {
    fn open(&self, protocol: &str, metadata: &[u8]) -> Result<Box<dyn LinkStream>, Error> {
        self.link
            .get()
            .open(protocol, &self.named(protocol, metadata)?)
    }

    fn open_with(
        &self,
        protocol: &str,
        metadata: &[u8],
        options: &OpenOptions,
    ) -> Result<Box<dyn LinkStream>, Error> {
        self.link
            .get()
            .open_with(protocol, &self.named(protocol, metadata)?, options)
    }

    fn bulk_window(&self) -> Option<u32> {
        self.link.get().bulk_window()
    }

    fn is_up(&self) -> bool {
        self.link.get().is_up()
    }

    fn open_cause(
        &self,
        protocol: &str,
        metadata: &[u8],
    ) -> Result<(Box<dyn LinkStream>, connect::Cause), Error> {
        self.link
            .get()
            .open_cause(protocol, &self.named(protocol, metadata)?)
    }

    fn relink_wait(&self) -> Option<std::time::Duration> {
        self.link.get().relink_wait()
    }

    fn files_source(&self) -> Option<&str> {
        Some(&self.name)
    }
}
