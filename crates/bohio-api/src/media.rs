// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The media kind: virtual media (a disc image the system boots or installs from).

use guatiao::library::{Object, ProviderError};

use crate::model::{MediaSlot, SourceInfo};
use crate::unsupported;

/// A source of virtual media.
#[guatiao::kind(name = "bohio_media")]
pub trait MediaSource: Send + Sync {
    /// What this source is and can do now; `offers` lists `status`, `insert_url`, `insert_reader` and `eject`.
    ///
    /// # Errors
    ///
    /// Only a source that cannot even describe itself.
    fn describe(&self) -> Result<SourceInfo, ProviderError>;

    /// The slots and what is inserted in each.
    ///
    /// # Errors
    ///
    /// `GONE` when the source cannot be reached.
    fn status(&self) -> Result<Vec<MediaSlot>, ProviderError>;

    /// Inserts the image at `url`, which the source (a BMC) fetches itself.
    ///
    /// # Errors
    ///
    /// `UNSUPPORTED` when the source cannot fetch, `NOT_FOUND` for a slot it does not have, `BUSY` when the slot holds one.
    fn insert_url(&self, slot: &str, url: &str) -> Result<MediaSlot, ProviderError> {
        let _ = (slot, url);
        Err(unsupported("this source does not fetch images from a URL"))
    }

    /// Inserts an image the host serves: the source reads it through `reader` for as long as it is inserted.
    ///
    /// # Errors
    ///
    /// `UNSUPPORTED` when the source cannot read from the host.
    fn insert_reader(
        &self,
        slot: &str,
        name: &str,
        reader: Object<dyn BlockReader>,
    ) -> Result<MediaSlot, ProviderError> {
        let _ = (slot, name, reader);
        Err(unsupported(
            "this source does not read images from the host",
        ))
    }

    /// Ejects what `slot` holds.
    ///
    /// # Errors
    ///
    /// `NOT_FOUND` for a slot the source does not have.
    fn eject(&self, slot: &str) -> Result<MediaSlot, ProviderError>;
}

/// A file the host lets a media source read: confined by the host, read-only, of a size the host reports.
#[guatiao::kind(object, name = "bohio_block_reader")]
pub trait BlockReader: Send + Sync {
    /// The size in bytes.
    ///
    /// # Errors
    ///
    /// `INTERNAL` when the file cannot be measured.
    fn size(&self) -> Result<u64, ProviderError>;

    /// Reads at `offset` into `dst` and answers the bytes read; fewer than `dst` only at the end.
    ///
    /// # Errors
    ///
    /// `INTERNAL` for an I/O failure.
    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<u64, ProviderError>;
}
