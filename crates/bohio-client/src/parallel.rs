//! One file over several streams: ranged reads and writes in parallel.
//!
//! A stream carries at most its receive window in flight, so one stream moves at most a window per round
//! trip; `n` streams on one link carry `n` windows. A link is one TCP connection with one congestion
//! window, so a path that loses packets gains from several links, each with its own proof: pass more
//! than one [`Open`] and the streams are spread over them.
//!
//! [`get_parallel`] reads ranges on `n` streams into a temporary sibling, each at its offset, checks the
//! whole file against the host's digest (`bohio-hash/1`) and renames it into place. [`put_parallel`]
//! creates a staged file beside the target, writes ranges to it on `n` `at` streams, checks the digest
//! and moves it over the target; a failure removes the staged file. [`read_parallel`] hands the ranges
//! to a writer in order, holding at most a few ranges in memory.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::hash::{BuildHasher, Hasher as _};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use bohio_proto::compress::Compression;
use bohio_proto::fs::{FileKind, ReadRequest, WriteMode, WriteRequest};
use bohio_proto::hash::{Algorithm, Digests};
use bohio_proto::{ErrorBody, ErrorCode};
use bohio_transfer::compress::Caps;
use bohio_transfer::hash::Hasher;

use crate::fs::{Fs, ReadOptions, Reader, Writer};
use crate::hash::hash_file;
use crate::sync::{Moved, Part};
use crate::{Error, Open};

/// How a parallel transfer divides and checks a file.
#[derive(Debug, Clone)]
pub struct ParallelOptions {
    /// How many streams run at once, spread over the links in turn. At least 1. Default 4.
    pub streams: usize,
    /// The bytes of one range, one stream each time it is taken up; a stream that finishes takes the next. `None`:
    /// [`get_parallel`] and [`put_parallel`] split the file into one range per stream, which opens each stream once;
    /// [`read_parallel`] uses [`DEFAULT_READ_RANGE`], because it holds ranges in memory. A smaller range balances a slow
    /// stream against a fast one and costs one open (a round trip) per range.
    pub range: Option<u64>,
    /// How a read asks the host to compress (anything the host accepts, `auto` included) or a write compresses (a
    /// concrete algorithm). Each range is its own compressed stream. Default none.
    pub compression: Option<Compression>,
    /// The levels and window a write compresses within.
    pub caps: Caps,
    /// The digests of the whole file that must agree between the two sides before it is put in place; the host reads the
    /// file once more to compute them. Empty skips the check, which saves that read and leaves a changed or damaged file
    /// undetected. Default `sha256`, which every host computes.
    pub hashes: Vec<Algorithm>,
    /// How long a step may wait for the host; `None` waits without limit. Default five minutes.
    pub timeout: Option<Duration>,
    /// The most bytes in one frame of a write. Default [`Writer::DEFAULT_CHUNK`].
    pub chunk: usize,
    /// The bytes of a local file read at a time. At least 1. Default 1 MiB.
    pub read_size: usize,
}

/// The range [`read_parallel`] uses when [`ParallelOptions::range`] is absent: 8 MiB.
pub const DEFAULT_READ_RANGE: u64 = 8 << 20;

impl Default for ParallelOptions {
    fn default() -> Self {
        Self {
            streams: 4,
            range: None,
            compression: None,
            caps: Caps {
                window_log_max: 27,
                ..Caps::default()
            },
            hashes: vec![Algorithm::Sha256],
            timeout: Some(Duration::from_secs(300)),
            chunk: Writer::DEFAULT_CHUNK,
            read_size: 1 << 20,
        }
    }
}

fn bad(message: &str) -> Error {
    Error::Remote(ErrorBody::new(ErrorCode::BadRequest, message))
}

/// The ranges `(offset, length)` that cover `size` bytes: `range` bytes each, or one per stream.
fn ranges(size: u64, options: &ParallelOptions, default: Option<u64>) -> Vec<(u64, u64)> {
    if size == 0 {
        return Vec::new();
    }
    let each = options
        .range
        .or(default)
        .unwrap_or_else(|| size.div_ceil(options.streams.max(1) as u64))
        .max(1);
    let mut all = Vec::new();
    let mut offset = 0;
    while offset < size {
        let length = each.min(size - offset);
        all.push((offset, length));
        offset += length;
    }
    all
}

fn pick<'a>(links: &[&'a dyn Open], worker: usize) -> Result<&'a dyn Open, Error> {
    if links.is_empty() {
        return Err(bad("a parallel transfer needs at least one link"));
    }
    Ok(links[worker % links.len()])
}

/// The first failure of the workers, kept for the caller; later ones are dropped.
#[derive(Default)]
struct Failed {
    stop: AtomicBool,
    first: Mutex<Option<Error>>,
}

impl Failed {
    fn set(&self, error: Error) {
        self.stop.store(true, Ordering::SeqCst);
        let mut first = self
            .first
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        first.get_or_insert(error);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    fn take(&self) -> Option<Error> {
        self.first
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }
}

fn remote_size(
    link: &dyn Open,
    path: &str,
    timeout: Option<Duration>,
) -> Result<(u64, Option<u32>), Error> {
    let fs = Fs::connect(link)?.with_timeout(timeout);
    let found = fs.stat(path, true);
    fs.control().close();
    let meta = found?;
    if meta.kind != FileKind::File {
        return Err(Error::Remote(ErrorBody::new(
            ErrorCode::BadRequest,
            "the path is not a regular file",
        )));
    }
    Ok((meta.size, meta.mode))
}

/// Reads `remote` into `local` on `options.streams` streams over `links`, each range at its offset in a temporary file
/// beside `local`; the whole file is checked against the host's digests (`options.hashes`) and then renamed over `local`,
/// which keeps its permissions. Nothing is left behind on failure.
///
/// # Errors
///
/// [`Error::Remote`] (`not_found`, `denied`, a path that is not a regular file, ...), [`Error::Local`] for the local
/// file, [`Error::Mismatch`] when the digests differ (`local` is untouched), the transport's errors.
pub fn get_parallel(
    links: &[&dyn Open],
    remote: &str,
    local: &Path,
    options: &ParallelOptions,
) -> Result<Moved, Error> {
    let first = pick(links, 0)?;
    let (size, _) = remote_size(first, remote, options.timeout)?;
    let like = fs::metadata(local).ok().filter(fs::Metadata::is_file);
    let part = Part::create(local, 1)?;
    part.file()
        .set_len(size)
        .map_err(|error| Error::local("sizing the temporary file", &error))?;
    let plan = ranges(size, options, None);
    let next = AtomicUsize::new(0);
    let wire = AtomicU64::new(0);
    let failed = Failed::default();
    let mut theirs: Result<Digests, Error> = Ok(Digests::new());
    std::thread::scope(|scope| {
        let check = (!options.hashes.is_empty())
            .then(|| scope.spawn(|| hash_file(first, remote, &options.hashes)));
        for worker in 0..options.streams.max(1).min(plan.len()) {
            let (plan, next, wire, failed, part) = (&plan, &next, &wire, &failed, &part);
            scope.spawn(move || {
                let outcome = pull_ranges(
                    pick(links, worker),
                    remote,
                    part.path(),
                    plan,
                    next,
                    wire,
                    failed,
                    options,
                );
                if let Err(error) = outcome {
                    failed.set(error);
                }
            });
        }
        if let Some(check) = check {
            theirs = check.join().unwrap_or(Err(Error::Closed));
        }
    });
    if let Some(error) = failed.take() {
        return Err(error);
    }
    let theirs = theirs?;
    if !options.hashes.is_empty() {
        let mine = digest_file(part.path(), &options.hashes, options.read_size)?;
        if mine != theirs {
            return Err(Error::Mismatch);
        }
    }
    part.put_in_place(local, like.as_ref())?;
    Ok(Moved {
        size,
        literal_bytes: size,
        copied_bytes: 0,
        wire_bytes: wire.load(Ordering::SeqCst),
        delta: false,
        skipped: false,
    })
}

#[allow(clippy::too_many_arguments, reason = "the worker's shared state")]
fn pull_ranges(
    link: Result<&dyn Open, Error>,
    remote: &str,
    temporary: &Path,
    plan: &[(u64, u64)],
    next: &AtomicUsize,
    wire: &AtomicU64,
    failed: &Failed,
    options: &ParallelOptions,
) -> Result<(), Error> {
    let link = link?;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .open(temporary)
        .map_err(|error| Error::local("opening the temporary file", &error))?;
    loop {
        let index = next.fetch_add(1, Ordering::SeqCst);
        let Some(&(offset, length)) = plan.get(index) else {
            return Ok(());
        };
        if failed.stopped() {
            return Ok(());
        }
        let mut request = ReadRequest::whole(remote);
        request.offset = offset;
        request.length = Some(length);
        request.compression.clone_from(&options.compression);
        let reader = Reader::start_with(link, &request, options.timeout, ReadOptions::default())?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| Error::local("writing the temporary file", &error))?;
        let mut got = 0_u64;
        while let Some(chunk) = reader.next_chunk(options.timeout)? {
            if failed.stopped() {
                return Ok(());
            }
            file.write_all(&chunk)
                .map_err(|error| Error::local("writing the temporary file", &error))?;
            got += chunk.len() as u64;
        }
        if got != length {
            return Err(Error::Closed);
        }
        wire.fetch_add(reader.wire_bytes(), Ordering::SeqCst);
    }
}

fn digest_file(path: &Path, algorithms: &[Algorithm], read_size: usize) -> Result<Digests, Error> {
    let mut hasher =
        Hasher::new(algorithms).map_err(|error| Error::Compression(error.to_string()))?;
    let mut file =
        File::open(path).map_err(|error| Error::local("reading the local file", &error))?;
    let mut buffer = vec![0_u8; read_size.max(1)];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => return Ok(hasher.finish()),
            Ok(count) => hasher.update(&buffer[..count]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(Error::local("reading the local file", &error)),
        }
    }
}

/// A file staged on the host beside its target: removed unless it was moved into place.
struct Staged<'a> {
    link: &'a dyn Open,
    path: String,
    timeout: Option<Duration>,
    moved: bool,
}

impl Drop for Staged<'_> {
    fn drop(&mut self) {
        if self.moved {
            return;
        }
        if let Ok(fs) = Fs::connect(self.link) {
            let fs = fs.with_timeout(self.timeout);
            let _ = fs.remove(&self.path, false);
            fs.control().close();
        }
    }
}

/// `.<name>.bohio-part-<16 hex>` beside `remote`, in the host's own path text (the last `/` or `\` ends the directory).
fn staged_name(remote: &str) -> String {
    let at = remote.rfind(['/', '\\']).map_or(0, |at| at + 1);
    let (directory, name) = remote.split_at(at);
    let random = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    format!("{directory}.{name}.bohio-part-{random:016x}")
}

/// Sends `local` to `remote` on `options.streams` streams over `links`: a staged file `.<name>.bohio-part-<16 hex>` is
/// created beside the target, each range is written to it by an `at` stream, the whole file is checked against the local
/// digests (`options.hashes`), and the staged file is moved over `remote` (`overwrite`, keeping the target's permissions
/// when it existed). A failure removes the staged file.
///
/// # Errors
///
/// As [`Writer::start`] and [`Writer::finish`]; [`Error::Local`] for the local file, [`Error::Mismatch`] when the
/// digests differ (the target is untouched).
pub fn put_parallel(
    links: &[&dyn Open],
    local: &Path,
    remote: &str,
    options: &ParallelOptions,
) -> Result<Moved, Error> {
    let first = pick(links, 0)?;
    let here =
        fs::metadata(local).map_err(|error| Error::local("reading the local file", &error))?;
    if !here.is_file() {
        return Err(Error::Local(
            "the local path is not a regular file".to_owned(),
        ));
    }
    let size = here.len();
    let mode = {
        let fs = Fs::connect(first)?.with_timeout(options.timeout);
        let found = fs.stat(remote, false);
        fs.control().close();
        match found {
            Ok(meta) if meta.kind == FileKind::File => meta.mode,
            Ok(_) => None,
            Err(Error::Remote(body)) if body.code == ErrorCode::NotFound => None,
            Err(error) => return Err(error),
        }
    };
    let staged = staged_name(remote);
    let mut create = WriteRequest::replace(staged.clone());
    create.mode = WriteMode::Create;
    create.file_mode = mode;
    create.hashes = Some(Vec::new());
    Writer::start(first, &create, options.timeout)?.finish(options.timeout)?;
    let mut guard = Staged {
        link: first,
        path: staged.clone(),
        timeout: options.timeout,
        moved: false,
    };
    let plan = ranges(size, options, None);
    let next = AtomicUsize::new(0);
    let wire = AtomicU64::new(0);
    let failed = Failed::default();
    let mut mine: Result<Digests, Error> = Ok(Digests::new());
    std::thread::scope(|scope| {
        let check = (!options.hashes.is_empty())
            .then(|| scope.spawn(|| digest_file(local, &options.hashes, options.read_size)));
        for worker in 0..options.streams.max(1).min(plan.len()) {
            let (plan, next, wire, failed, staged) = (&plan, &next, &wire, &failed, &staged);
            scope.spawn(move || {
                let outcome = push_ranges(
                    pick(links, worker),
                    local,
                    staged,
                    plan,
                    next,
                    wire,
                    failed,
                    options,
                );
                if let Err(error) = outcome {
                    failed.set(error);
                }
            });
        }
        if let Some(check) = check {
            mine = check.join().unwrap_or(Err(Error::Closed));
        }
    });
    if let Some(error) = failed.take() {
        return Err(error);
    }
    let mine = mine?;
    if !options.hashes.is_empty() && hash_file(first, &staged, &options.hashes)? != mine {
        return Err(Error::Mismatch);
    }
    let fs = Fs::connect(first)?.with_timeout(options.timeout);
    let put = fs.move_to(&staged, remote, true);
    fs.control().close();
    put?;
    guard.moved = true;
    Ok(Moved {
        size,
        literal_bytes: size,
        copied_bytes: 0,
        wire_bytes: wire.load(Ordering::SeqCst),
        delta: false,
        skipped: false,
    })
}

#[allow(clippy::too_many_arguments, reason = "the worker's shared state")]
fn push_ranges(
    link: Result<&dyn Open, Error>,
    local: &Path,
    staged: &str,
    plan: &[(u64, u64)],
    next: &AtomicUsize,
    wire: &AtomicU64,
    failed: &Failed,
    options: &ParallelOptions,
) -> Result<(), Error> {
    let link = link?;
    let mut file =
        File::open(local).map_err(|error| Error::local("opening the local file", &error))?;
    let mut buffer = vec![0_u8; options.chunk.max(1).min(options.read_size.max(1))];
    loop {
        let index = next.fetch_add(1, Ordering::SeqCst);
        let Some(&(offset, length)) = plan.get(index) else {
            return Ok(());
        };
        if failed.stopped() {
            return Ok(());
        }
        let mut request = WriteRequest::replace(staged);
        request.mode = WriteMode::At { offset };
        request.compression.clone_from(&options.compression);
        request.hashes = Some(Vec::new());
        let writer = Writer::start_with(link, &request, options.timeout, &options.caps)?
            .with_chunk(options.chunk.max(1));
        file.seek(SeekFrom::Start(offset))
            .map_err(|error| Error::local("reading the local file", &error))?;
        let mut left = length;
        while left > 0 {
            if failed.stopped() {
                return Ok(());
            }
            let want = buffer
                .len()
                .min(usize::try_from(left).unwrap_or(usize::MAX));
            let count = match file.read(&mut buffer[..want]) {
                Ok(0) => return Err(Error::Local("the local file ended early".to_owned())),
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(Error::local("reading the local file", &error)),
            };
            writer.write(&buffer[..count])?;
            left -= count as u64;
        }
        let done = writer.finish(options.timeout)?;
        if done.written != length {
            return Err(Error::Closed);
        }
        wire.fetch_add(writer.wire_bytes(), Ordering::SeqCst);
    }
}

/// The ranges [`read_parallel`] has fetched and not yet written, and where each side is.
struct Ahead {
    ready: BTreeMap<usize, Vec<u8>>,
    /// The next range to hand a worker.
    next: usize,
    /// The next range the writer needs.
    wanted: usize,
}

/// Reads the whole of `remote` on `options.streams` streams over `links` and writes it to `out` in order. Ranges are
/// fetched ahead of the writer, at most two per stream, so memory is that many ranges ([`DEFAULT_READ_RANGE`] each unless
/// `options.range` says). Returns the bytes written; with `options.hashes` the digests of what was written are checked
/// against the host's after the last byte, which is too late to un-write them.
///
/// # Errors
///
/// [`Error::Remote`], [`Error::Local`] for `out`, [`Error::Mismatch`] for digests that differ, the transport's errors.
pub fn read_parallel(
    links: &[&dyn Open],
    remote: &str,
    out: &mut dyn Write,
    options: &ParallelOptions,
) -> Result<u64, Error> {
    let first = pick(links, 0)?;
    let (size, _) = remote_size(first, remote, options.timeout)?;
    let plan = ranges(size, options, Some(DEFAULT_READ_RANGE));
    let streams = options.streams.max(1);
    let window = streams.saturating_mul(2);
    let state = Mutex::new(Ahead {
        ready: BTreeMap::new(),
        next: 0,
        wanted: 0,
    });
    let changed = Condvar::new();
    let failed = Failed::default();
    let mut hasher =
        Hasher::new(&options.hashes).map_err(|error| Error::Compression(error.to_string()))?;
    let mut written = 0_u64;
    let mut theirs: Result<Digests, Error> = Ok(Digests::new());
    std::thread::scope(|scope| {
        let check = (!options.hashes.is_empty())
            .then(|| scope.spawn(|| hash_file(first, remote, &options.hashes)));
        for worker in 0..streams.min(plan.len()) {
            let (plan, state, changed, failed) = (&plan, &state, &changed, &failed);
            scope.spawn(move || {
                let outcome = fetch_ranges(
                    pick(links, worker),
                    remote,
                    plan,
                    window,
                    state,
                    changed,
                    failed,
                    options,
                );
                if let Err(error) = outcome {
                    failed.set(error);
                    wake(state, changed);
                }
            });
        }
        for index in 0..plan.len() {
            let chunk = {
                let mut held = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                loop {
                    if failed.stopped() {
                        break None;
                    }
                    if let Some(chunk) = held.ready.remove(&index) {
                        held.wanted = index + 1;
                        changed.notify_all();
                        break Some(chunk);
                    }
                    held = changed
                        .wait(held)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
            };
            let Some(chunk) = chunk else { break };
            if let Err(error) = out.write_all(&chunk) {
                failed.set(Error::local("writing the output", &error));
                wake(&state, &changed);
                break;
            }
            hasher.update(&chunk);
            written += chunk.len() as u64;
        }
        if let Some(check) = check {
            theirs = check.join().unwrap_or(Err(Error::Closed));
        }
    });
    if let Some(error) = failed.take() {
        return Err(error);
    }
    out.flush()
        .map_err(|error| Error::local("writing the output", &error))?;
    if !options.hashes.is_empty() && hasher.finish() != theirs? {
        return Err(Error::Mismatch);
    }
    Ok(written)
}

/// Wakes everything waiting on `changed` after a flag outside the lock was set: taking the lock first means no waiter is
/// between its check of the flag and its wait.
fn wake(state: &Mutex<Ahead>, changed: &Condvar) {
    drop(
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    );
    changed.notify_all();
}

#[allow(clippy::too_many_arguments, reason = "the worker's shared state")]
fn fetch_ranges(
    link: Result<&dyn Open, Error>,
    remote: &str,
    plan: &[(u64, u64)],
    window: usize,
    state: &Mutex<Ahead>,
    changed: &Condvar,
    failed: &Failed,
    options: &ParallelOptions,
) -> Result<(), Error> {
    let link = link?;
    loop {
        let index = {
            let mut held = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            loop {
                if failed.stopped() || held.next >= plan.len() {
                    return Ok(());
                }
                if held.next < held.wanted.saturating_add(window) {
                    held.next += 1;
                    break held.next - 1;
                }
                held = changed
                    .wait(held)
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
            }
        };
        let (offset, length) = plan[index];
        let mut request = ReadRequest::whole(remote);
        request.offset = offset;
        request.length = Some(length);
        request.compression.clone_from(&options.compression);
        let reader = Reader::start_with(link, &request, options.timeout, ReadOptions::default())?;
        let mut chunk = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
        while let Some(piece) = reader.next_chunk(options.timeout)? {
            if failed.stopped() {
                return Ok(());
            }
            chunk.extend_from_slice(&piece);
        }
        if chunk.len() as u64 != length {
            return Err(Error::Closed);
        }
        let mut held = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        held.ready.insert(index, chunk);
        changed.notify_all();
    }
}
