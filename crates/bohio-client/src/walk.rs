//! The entries of whole trees on a host, on one stream: `bohio-walk/1`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use bohio_proto::compress::Compression;
use bohio_proto::hash::Algorithm;
use bohio_proto::protocol;
use bohio_proto::walk::{
    WalkDone, WalkEntries, WalkEntry, WalkInput, WalkItemError, WalkOutput, WalkRequest, WalkRoot,
};
use bohio_transfer::compress::{Bounds, Decoder};
use coqui_link::mux::LinkStream;

use crate::{Error, Open};

/// What a walk lists and how the host sends it.
#[derive(Debug, Clone, Default)]
pub struct WalkOptions {
    /// How far below a root to go: `0` is the roots themselves (a batched `stat`), `None` the whole tree.
    pub depth: Option<u64>,
    /// Enter a link to a directory and describe what a link to a file points at, inside the root only.
    pub follow: bool,
    /// Only entries whose path or name matches one of these globs are listed (directories are still entered).
    pub include: Vec<String>,
    /// Entries whose path or name matches one of these globs are left out, and a directory's whole subtree.
    pub exclude: Vec<String>,
    /// Digests of each regular file's content, computed on the host.
    pub hashes: Vec<Algorithm>,
    /// How the host compresses the entries; `None` sends them raw. `auto` is the host's default.
    pub compression: Option<Compression>,
}

/// One thing a walk found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkItem {
    /// An entry of a tree.
    Entry {
        /// Which root: its position among the roots given to [`walk`], from `0`.
        root: u64,
        /// The entry.
        entry: Box<WalkEntry>,
    },
    /// A directory or root the host could not read (the walk went on), or the host's own bound.
    Failure(WalkItemError),
}

/// The entries of a walk, as they arrive. Dropping it closes the stream, which stops the host.
pub struct WalkIter {
    channel: Arc<dyn LinkStream>,
    decoder: Option<Decoder>,
    pending: VecDeque<WalkItem>,
    done: Option<WalkDone>,
    ended: bool,
    wire: AtomicU64,
    compression: Option<Compression>,
    sender: Option<JoinHandle<()>>,
    timeout: Option<Duration>,
}

impl core::fmt::Debug for WalkIter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WalkIter")
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

/// Starts a walk of `roots` on the host.
///
/// The roots are sent while the entries come back, so any number of them is fine, and the host answers
/// them in order. Each root's first entry is the root itself, with an empty path.
///
/// # Errors
///
/// [`Error::Remote`] when the host refused the stream (`denied`, `busy` at its walk-stream limit,
/// `unsupported` for a compression or an algorithm it does not offer, `bad_request` for a glob that does not
/// parse), [`Error::Closed`], or whatever `link` reports. Failures of what is walked come as
/// [`WalkItem::Failure`].
pub fn walk(link: &dyn Open, roots: &[&str], options: &WalkOptions) -> Result<WalkIter, Error> {
    walk_with(link, roots, options, Some(Duration::from_secs(300)))
}

/// As [`walk`], waiting at most `timeout` for each frame (`None` waits without limit).
///
/// # Errors
///
/// As [`walk`].
pub fn walk_with(
    link: &dyn Open,
    roots: &[&str],
    options: &WalkOptions,
    timeout: Option<Duration>,
) -> Result<WalkIter, Error> {
    let request = WalkRequest {
        source: None,
        depth: options.depth,
        follow: options.follow,
        include: options.include.clone(),
        exclude: options.exclude.clone(),
        hashes: options.hashes.clone(),
        compression: options.compression.clone(),
    };
    let channel: Arc<dyn LinkStream> =
        Arc::from(link.open(protocol::WALK, &request.to_metadata()?)?);
    // The first frame says what the host will do, or why it will not.
    let header = loop {
        let Some(frame) = channel.receive(timeout)? else {
            return Err(Error::Closed);
        };
        match WalkOutput::decode(&frame) {
            Ok(WalkOutput::Header(header)) => break header,
            Ok(WalkOutput::Error(body)) => return Err(Error::Remote(body)),
            Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
            Err(error) => return Err(error.into()),
        }
    };
    let compression = header
        .compression
        .filter(|used| used.algorithm.compresses());
    let decoder = compression
        .as_ref()
        .map(|used| {
            Decoder::new(
                used,
                &Bounds {
                    window_log_max: 27,
                    max_output: None,
                    max_expansion: 0,
                },
            )
        })
        .transpose()
        .map_err(|error| Error::Compression(error.to_string()))?;
    let paths: Vec<String> = roots.iter().map(|root| (*root).to_owned()).collect();
    let sending = Arc::clone(&channel);
    let sender = std::thread::spawn(move || {
        for path in paths {
            let Ok(frame) = WalkInput::Root(WalkRoot { path }).encode() else {
                return;
            };
            if sending.send_timeout(&frame, None).is_err() {
                return;
            }
        }
        if let Ok(frame) = WalkInput::End.encode() {
            let _ = sending.send_timeout(&frame, None);
        }
    });
    Ok(WalkIter {
        channel,
        decoder,
        pending: VecDeque::new(),
        done: None,
        ended: false,
        wire: AtomicU64::new(0),
        compression,
        sender: Some(sender),
        timeout,
    })
}

impl WalkIter {
    /// The host's closing count, once the walk has ended.
    pub const fn done(&self) -> Option<WalkDone> {
        self.done
    }

    /// How many bytes of frames arrived so far, as they travelled (compressed where they were).
    pub fn wire_bytes(&self) -> u64 {
        self.wire.load(Ordering::Relaxed)
    }

    /// What the host compresses the entries with: `None` for raw entries.
    pub const fn compression(&self) -> Option<&Compression> {
        self.compression.as_ref()
    }

    fn take(&mut self, entries: WalkEntries) {
        for entry in entries.entries {
            self.pending.push_back(WalkItem::Entry {
                root: entries.root,
                entry: Box::new(entry),
            });
        }
    }

    fn next_frame(&mut self) -> Result<(), Error> {
        let Some(frame) = self.channel.receive(self.timeout)? else {
            return Err(Error::Closed);
        };
        self.wire.fetch_add(frame.len() as u64, Ordering::Relaxed);
        match WalkOutput::decode(&frame) {
            Ok(WalkOutput::Entries(entries)) => {
                // A raw frame ends the compression context.
                if let Some(decoder) = self.decoder.as_mut() {
                    decoder.reset();
                }
                self.take(entries);
            }
            Ok(WalkOutput::Compressed(piece)) => {
                let decoder = self
                    .decoder
                    .as_mut()
                    .ok_or(Error::Protocol(bohio_proto::Error::Decode))?;
                let mut body = Vec::new();
                decoder
                    .feed(&piece, &mut body)
                    .map_err(|error| Error::Compression(error.to_string()))?;
                let entries: WalkEntries = bohio_proto::decode(&body)?;
                self.take(entries);
            }
            Ok(WalkOutput::ItemError(error)) => self.pending.push_back(WalkItem::Failure(error)),
            Ok(WalkOutput::Done(done)) => {
                self.done = Some(done);
                self.ended = true;
            }
            Ok(WalkOutput::Error(body)) => {
                self.ended = true;
                return Err(Error::Remote(body));
            }
            Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
}

impl Iterator for WalkIter {
    type Item = Result<WalkItem, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if let Some(item) = self.pending.pop_front() {
                return Some(Ok(item));
            }
            if self.ended {
                return None;
            }
            if let Err(error) = self.next_frame() {
                self.ended = true;
                return Some(Err(error));
            }
        }
    }
}

impl Drop for WalkIter {
    fn drop(&mut self) {
        // Closing ends a send still waiting for room, which a host that gave up no longer makes.
        self.channel.close();
        if let Some(sender) = self.sender.take() {
            let _ = sender.join();
        }
    }
}
