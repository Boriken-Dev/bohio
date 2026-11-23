//! A remote file as `Read + Seek`: bounded ranges fetched ahead, so a reader that seeks and reads small pieces does not
//! pay a round trip for each.

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::time::Duration;

use bohio_proto::compress::Compression;
use bohio_proto::fs::ReadRequest;

use crate::fs::Reader;
use crate::{Error, Open};

/// How a [`RangeReader`] fetches.
#[derive(Debug, Clone)]
pub struct RangeOptions {
    /// The bytes one range holds (and one read stream asks for). At least 1. Default 1 MiB.
    pub range: u64,
    /// How many ranges beyond the one being read are already requested, so their bytes travel while this one is
    /// consumed. `0` requests each range when the reader gets there. Default 2.
    pub prefetch: usize,
    /// How the host compresses what it sends; `None` sends it raw.
    pub compression: Option<Compression>,
    /// How long a step may wait for the host; `None` waits without limit. Default five minutes.
    pub timeout: Option<Duration>,
}

impl Default for RangeOptions {
    fn default() -> Self {
        Self {
            range: 1 << 20,
            prefetch: 2,
            compression: None,
            timeout: Some(Duration::from_secs(300)),
        }
    }
}

/// A range being fetched: its stream is open and its bytes are on their way.
struct Ahead {
    start: u64,
    reader: Reader,
}

/// A file of a host that can be read and sought like a local one.
///
/// A read takes its bytes from the range that holds the position; a range is one `bohio-read/1` stream of
/// `options.range` bytes, and the next `options.prefetch` ranges are already open. A seek inside the range held, or
/// to the start of one that is open, costs nothing; a seek anywhere else closes the streams ahead, which wastes at
/// most `range * prefetch` bytes in flight, and opens new ones from the new position. The size is the file's when the reader
/// opened: a file that changes while it is read gives bytes of both versions.
pub struct RangeReader<'a> {
    link: &'a dyn Open,
    path: String,
    options: RangeOptions,
    size: u64,
    position: u64,
    block: Option<(u64, Vec<u8>)>,
    ahead: VecDeque<Ahead>,
}

impl core::fmt::Debug for RangeReader<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RangeReader")
            .field("path", &self.path)
            .field("size", &self.size)
            .field("position", &self.position)
            .finish_non_exhaustive()
    }
}

impl<'a> RangeReader<'a> {
    /// Opens `path`: the first range is requested, and its header says how large the file is.
    ///
    /// # Errors
    ///
    /// [`Error::Remote`] (`not_found`, `denied`, `outside_root`, `unsupported` for a compression the host does not
    /// offer, ...), [`Error::Timeout`], [`Error::Closed`].
    pub fn open(link: &'a dyn Open, path: &str, options: &RangeOptions) -> Result<Self, Error> {
        let options = RangeOptions {
            range: options.range.max(1),
            ..options.clone()
        };
        let mut reader = Self {
            link,
            path: path.to_owned(),
            options,
            size: 0,
            position: 0,
            block: None,
            ahead: VecDeque::new(),
        };
        let first = reader.request(0)?;
        reader.size = first.reader.header().size;
        reader.ahead.push_back(first);
        reader.top_up()?;
        Ok(reader)
    }

    /// The file's size when this reader opened.
    pub const fn len(&self) -> u64 {
        self.size
    }

    /// Whether the file was empty when this reader opened.
    pub const fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Where the next read starts.
    pub const fn position(&self) -> u64 {
        self.position
    }

    fn request(&self, start: u64) -> Result<Ahead, Error> {
        let mut request = ReadRequest::whole(&self.path);
        request.offset = start;
        request.length = Some(self.options.range);
        request.compression.clone_from(&self.options.compression);
        let reader = Reader::start(self.link, &request, self.options.timeout)?;
        Ok(Ahead { start, reader })
    }

    /// Opens ranges after the last one open until `prefetch` are open beyond the first.
    fn top_up(&mut self) -> Result<(), Error> {
        while self.ahead.len() <= self.options.prefetch {
            let next = self
                .ahead
                .back()
                .map_or(self.position, |last| last.start + self.options.range);
            if next >= self.size {
                break;
            }
            let range = self.request(next)?;
            self.ahead.push_back(range);
        }
        Ok(())
    }

    /// Makes `block` hold the range that holds `position`.
    fn fill(&mut self) -> Result<(), Error> {
        if self.block.as_ref().is_some_and(|(start, bytes)| {
            self.position >= *start && self.position < *start + bytes.len() as u64
        }) {
            return Ok(());
        }
        // A range already open that starts here (a sequential read), else everything ahead is dropped and a
        // new one starts at the position.
        while self.ahead.front().is_some_and(|front| {
            front.start < self.position && front.start + self.options.range <= self.position
        }) {
            self.ahead.pop_front();
        }
        if self
            .ahead
            .front()
            .is_none_or(|front| front.start != self.position)
        {
            self.ahead.clear();
            let range = self.request(self.position)?;
            self.ahead.push_back(range);
        }
        let Some(front) = self.ahead.pop_front() else {
            return Err(Error::Closed);
        };
        let bytes = front.reader.read_to_end(self.options.timeout)?;
        self.block = Some((front.start, bytes));
        self.top_up_from(front.start)
    }

    fn top_up_from(&mut self, start: u64) -> Result<(), Error> {
        // The ranges ahead follow the one just read.
        let mut next = self
            .ahead
            .back()
            .map_or(start + self.options.range, |last| {
                last.start + self.options.range
            });
        while self.ahead.len() < self.options.prefetch && next < self.size {
            let range = self.request(next)?;
            self.ahead.push_back(range);
            next += self.options.range;
        }
        Ok(())
    }
}

fn io_error(error: &Error) -> io::Error {
    io::Error::other(error.to_string())
}

impl Read for RangeReader<'_> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() || self.position >= self.size {
            return Ok(0);
        }
        self.fill().map_err(|error| io_error(&error))?;
        let Some((start, bytes)) = &self.block else {
            return Ok(0);
        };
        let at = usize::try_from(self.position - start).unwrap_or(usize::MAX);
        let Some(available) = bytes.get(at..) else {
            // The file ended before its announced size: the host sent no more.
            return Ok(0);
        };
        let count = available.len().min(out.len());
        out[..count].copy_from_slice(&available[..count]);
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for RangeReader<'_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(offset) => Some(offset),
            SeekFrom::End(delta) => self.size.checked_add_signed(delta),
            SeekFrom::Current(delta) => self.position.checked_add_signed(delta),
        };
        let Some(target) = target else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a seek before the start of the file",
            ));
        };
        self.position = target;
        Ok(target)
    }
}
