//! Keeping a local file and a file of a host the same: compare without downloading, and move only what differs.
//!
//! [`compare_file`] decides from the sizes and times, then a digest of each side, then the blocks, and
//! [`put`] and [`get`] copy a file with a delta when the other side already has an older copy (rsync's algorithm,
//! specified in `docs/protocol.md`): the receiver's block signature goes one way and only the blocks it lacks come
//! back. Memory is a block signature and a few blocks, never the file.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bohio_proto::compress::{Codec, Compression};
use bohio_proto::fs::{
    DeltaBase, FileKind, Metadata, ReadDelta, ReadInput, ReadOutput, ReadRequest, SignatureBatch,
    Timestamp, WEAK_RSYNC, WriteMode, WriteRequest,
};
use bohio_proto::hash::{Algorithm, BlockOptions, Digests, HashItem};
use bohio_proto::info::TransferInfo;
use bohio_proto::{ErrorBody, ErrorCode, protocol};
use bohio_transfer::apply::{apply, copy_len};
use bohio_transfer::compress::{Bounds, Caps, Decoder};
use bohio_transfer::delta::{Matcher, Op};
use bohio_transfer::hash::{Hasher, digest_reader};
use bohio_transfer::signature::{SignatureBuilder, SignatureSpec, block_size_for};
use coqui_transport::Channel;

use crate::control::Control;
use crate::fs::{Fs, ReadOptions, Reader, Writer};
use crate::hash::{hash_blocks, hash_file};
use crate::{Error, Open, OpenOptions};

/// How far [`compare_file`] goes to tell two files apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Compare {
    /// Sizes and modification times only: nothing is read. Two files of the same size whose times differ by
    /// any amount are different, and the times are compared to the nanosecond.
    Meta,
    /// Sizes, then the digest of each side by this algorithm (one `transfer.hashes` offers). The times are not looked at.
    Hash(Algorithm),
    /// Sizes, then the block signature of each side: every block's weak and strong hash must be equal.
    Blocks,
}

/// What [`compare_file`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Both files exist and hold the same bytes (as far as `how` looked).
    Same,
    /// Both exist and differ.
    Differs,
    /// There is no file on the host.
    RemoteMissing,
    /// There is no local file.
    LocalMissing,
}

/// Whether [`put`] and [`get`] use a delta.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeltaMode {
    /// A delta when the file already at the destination is at least the host's `[delta] min_size` (from `info`) and
    /// the host serves deltas; a whole transfer otherwise.
    #[default]
    Auto,
    /// A delta whenever the destination has the file and the host serves deltas, whatever its size.
    Always,
    /// Always the whole file.
    Never,
}

/// What [`put`] and [`get`] move and how.
#[derive(Debug, Clone)]
pub struct SyncOptions {
    /// Whether to use a delta. Default [`DeltaMode::Auto`].
    pub delta: DeltaMode,
    /// How to compress what is sent: a concrete algorithm for [`put`] (see `fs::resolve_auto`), anything the
    /// host accepts for [`get`] (`auto` is the host's default). Default none.
    pub compression: Option<Compression>,
    /// When set, a file that [`compare_file`] finds [`Verdict::Same`] by this depth is not moved. Default none.
    pub compare: Option<Compare>,
    /// The levels and window this side compresses within.
    pub caps: Caps,
    /// The block size of the signature this side makes (a `get`'s local file) or asks the host to make (a `put`'s remote
    /// file); the host's bounds and the file's size choose it when absent.
    pub block_size: Option<u32>,
    /// The strong hash of each block. Default `blake3`.
    pub strong: Algorithm,
    /// How many bytes of each strong hash a signature keeps. Default the digest's length up to 16.
    pub strong_len: Option<usize>,
    /// The digest that checks the whole file after it is rebuilt. Default `blake3`, else `sha256`, whichever the host offers.
    pub verify: Option<Algorithm>,
    /// The most bytes in one frame. Default 64 KiB.
    pub chunk: usize,
    /// How long a step may wait for the host; `None` waits without limit. Default five minutes.
    pub timeout: Option<Duration>,
    /// The bytes read from a local file at a time, and the buffer a downloaded file is written through: more is fewer
    /// system calls and that much more memory. At least 1. Default 1 MiB.
    pub read_size: usize,
    /// What the host offers (`info.transfer`, or the `hello` reply's), when the caller has it already: saves a call.
    pub transfer: Option<TransferInfo>,
    /// The largest zstd window a read accepts, as a power of two; 27 when absent.
    pub window_log_max: Option<u32>,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            delta: DeltaMode::Auto,
            compression: None,
            compare: None,
            caps: Caps {
                window_log_max: 27,
                ..Caps::default()
            },
            block_size: None,
            strong: Algorithm::Blake3,
            strong_len: None,
            verify: None,
            chunk: Writer::DEFAULT_CHUNK,
            timeout: Some(Duration::from_secs(300)),
            read_size: 1 << 20,
            transfer: None,
            window_log_max: None,
        }
    }
}

/// The options of [`put`].
pub type PutOptions = SyncOptions;

/// The options of [`get`].
pub type GetOptions = SyncOptions;

/// What a [`put`] or [`get`] moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Moved {
    /// The size of the file after the transfer.
    pub size: u64,
    /// The bytes of the file that travelled, before compression: all of them for a whole transfer.
    pub literal_bytes: u64,
    /// The bytes the receiving side already had and took from its old copy.
    pub copied_bytes: u64,
    /// The bytes of frames that travelled in both directions, as they travelled: the signature one way, the
    /// copies and the (compressed) literals the other.
    pub wire_bytes: u64,
    /// Whether a delta was used.
    pub delta: bool,
    /// The file was already the same by `compare`: nothing moved.
    pub skipped: bool,
}

fn timestamp(time: SystemTime) -> Timestamp {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => Timestamp {
            secs: i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
            nanos: after.subsec_nanos(),
        },
        Err(error) => {
            let before = error.duration();
            let secs = i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
            if before.subsec_nanos() == 0 {
                Timestamp {
                    secs: -secs,
                    nanos: 0,
                }
            } else {
                Timestamp {
                    secs: -secs - 1,
                    nanos: 1_000_000_000 - before.subsec_nanos(),
                }
            }
        }
    }
}

fn bad(message: &'static str) -> Error {
    Error::Remote(ErrorBody::new(ErrorCode::BadRequest, message))
}

/// The host's metadata for `path`, `None` when there is no such file.
fn remote_file(fs: &Fs, path: &str) -> Result<Option<Metadata>, Error> {
    match fs.stat(path, true) {
        Ok(meta) if meta.kind == FileKind::File => Ok(Some(meta)),
        Ok(_) => Err(bad("the path on the host is not a regular file")),
        Err(Error::Remote(body)) if body.code == ErrorCode::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The local file's metadata, `None` when there is no such file.
fn local_file(path: &Path) -> Result<Option<fs::Metadata>, Error> {
    match fs::metadata(path) {
        Ok(meta) if meta.is_file() => Ok(Some(meta)),
        Ok(_) => Err(Error::Local(
            "the local path is not a regular file".to_owned(),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Error::local("looking at the local file", &error)),
    }
}

/// What the host offers, from its `hello` reply: it carries `transfer` for a peer that does not hold `info`.
fn transfer_of(link: &dyn Open, options: &SyncOptions) -> Result<TransferInfo, Error> {
    if let Some(transfer) = &options.transfer {
        return Ok(transfer.clone());
    }
    let control = Control::open(link)?;
    let transfer = control.transfer(options.timeout);
    control.close();
    transfer
}

/// The digest that checks a rebuilt file: the caller's, else `blake3` or `sha256` if the host offers one, else any it offers.
fn verify_with(transfer: &TransferInfo, options: &SyncOptions) -> Option<Algorithm> {
    if options.verify.is_some() {
        return options.verify.clone();
    }
    let offered = &transfer.hashes;
    [Algorithm::Blake3, Algorithm::Sha256]
        .into_iter()
        .find(|algorithm| offered.contains(algorithm))
        .or_else(|| offered.first().cloned())
}

/// Whether the host serves what a delta needs.
fn host_does_deltas(transfer: &TransferInfo, options: &SyncOptions) -> bool {
    transfer.delta.weak.iter().any(|name| name == WEAK_RSYNC)
        && transfer.hashes.contains(&options.strong)
}

/// Whether to use a delta against a base file of `base_size` bytes.
fn wants_delta(transfer: &TransferInfo, options: &SyncOptions, base_size: u64) -> bool {
    match options.delta {
        DeltaMode::Never => false,
        DeltaMode::Always => host_does_deltas(transfer, options),
        DeltaMode::Auto => {
            host_does_deltas(transfer, options) && base_size >= transfer.delta.min_size
        }
    }
}

fn strong_len_of(options: &SyncOptions) -> Result<usize, Error> {
    options
        .strong_len
        .or_else(|| {
            SignatureSpec::digest_len(&options.strong).map(|digest| digest.min(DEFAULT_STRONG_LEN))
        })
        .ok_or_else(|| {
            Error::Compression("the strong hash is not an algorithm this build knows".to_owned())
        })
}

/// The bytes of a strong hash a signature keeps when `strong_len` is not set: a block's collision odds stay far below the
/// whole-file check's.
const DEFAULT_STRONG_LEN: usize = 16;

/// Compares a local file with a file of the host without downloading either, as far as `how` goes.
///
/// A local file that is missing is [`Verdict::LocalMissing`] (also when the host has none either), a host file that
/// is missing [`Verdict::RemoteMissing`]. Differing sizes are [`Verdict::Differs`] whatever `how` is.
///
/// # Errors
///
/// [`Error::Remote`] from the host (`denied`, `unsupported` for an algorithm it does not offer, ...);
/// [`Error::Local`] for a local file that cannot be read; the transport's errors.
pub fn compare_file(
    link: &dyn Open,
    local: &Path,
    remote: &str,
    how: Compare,
    options: &SyncOptions,
) -> Result<Verdict, Error> {
    let Some(here) = local_file(local)? else {
        return Ok(Verdict::LocalMissing);
    };
    let fs = Fs::connect(link)?.with_timeout(options.timeout);
    let there = remote_file(&fs, remote);
    fs.control().close();
    let Some(there) = there? else {
        return Ok(Verdict::RemoteMissing);
    };
    if here.len() != there.size {
        return Ok(Verdict::Differs);
    }
    let same = match how {
        Compare::Meta => {
            let modified = here.modified().ok().map(timestamp);
            modified.is_some() && modified == there.modified
        }
        Compare::Hash(algorithm) => {
            // The host first: it refuses an algorithm it does not offer before this side reads a byte.
            let remote_digests = hash_file(link, remote, std::slice::from_ref(&algorithm))?;
            let mut file = File::open(local)
                .map_err(|error| Error::local("opening the local file", &error))?;
            let (digests, _) = digest_reader(
                &mut file,
                std::slice::from_ref(&algorithm),
                options.read_size.max(1),
            )
            .map_err(|error| Error::local("reading the local file", &error))?;
            digests.contains_key(&algorithm)
                && digests.get(&algorithm) == remote_digests.get(&algorithm)
        }
        Compare::Blocks => {
            let blocks = hash_blocks(
                link,
                HashItem::whole(remote),
                std::slice::from_ref(&options.strong),
                &BlockOptions {
                    size: None,
                    weak: None,
                    strong_len: None,
                },
            )?;
            let spec = blocks.signature.spec().clone();
            let mut builder = SignatureBuilder::new(spec.clone())
                .map_err(|_| bad("the host chose a signature this build cannot make"))?;
            let mut mine = bohio_transfer::signature::Signature::new(spec, here.len())
                .map_err(|_| bad("the host chose a signature this build cannot make"))?;
            let mut file = File::open(local)
                .map_err(|error| Error::local("opening the local file", &error))?;
            let mut buffer = vec![0_u8; options.read_size.max(1)];
            loop {
                let count = match file.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => count,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(Error::local("reading the local file", &error)),
                };
                for block in builder.update(&buffer[..count]) {
                    mine.push(&block)
                        .map_err(|_| bad("the file grew while it was read"))?;
                }
            }
            if let Some(block) = builder.finish() {
                mine.push(&block)
                    .map_err(|_| bad("the file grew while it was read"))?;
            }
            mine == blocks.signature
        }
    };
    Ok(if same {
        Verdict::Same
    } else {
        Verdict::Differs
    })
}

fn skip_if_same(
    link: &dyn Open,
    local: &Path,
    remote: &str,
    options: &SyncOptions,
) -> Result<Option<Moved>, Error> {
    let Some(how) = options.compare.clone() else {
        return Ok(None);
    };
    if compare_file(link, local, remote, how, options)? != Verdict::Same {
        return Ok(None);
    }
    let size = fs::metadata(local).map_or(0, |meta| meta.len());
    Ok(Some(Moved {
        size,
        skipped: true,
        ..Moved::default()
    }))
}

/// Sends `local` to `remote` on the host, replacing it atomically: with a delta against the host's copy when
/// `options.delta` and the copy allow, else the whole file.
///
/// A delta asks the host for the signature of its copy (`bohio-hash/1`), matches the local file against it and sends
/// only the blocks the host lacks (`bohio-write/1` in mode `delta`), naming the copy as it saw it and the digest the new file must have;
/// the host refuses with `changed` when its copy moved meanwhile, and nothing is replaced. It needs `fs.read` as well as
/// `fs.write`.
///
/// # Errors
///
/// [`Error::Remote`] (`changed`, `denied`, `unsupported`, ...), [`Error::Local`] for the local file, the transport's errors.
pub fn put(
    link: &dyn Open,
    local: &Path,
    remote: &str,
    options: &PutOptions,
) -> Result<Moved, Error> {
    let here = local_file(local)?
        .ok_or_else(|| Error::Local("the local file does not exist".to_owned()))?;
    if let Some(moved) = skip_if_same(link, local, remote, options)? {
        return Ok(moved);
    }
    let transfer = transfer_of(link, options)?;
    let verify = verify_with(&transfer, options);
    let target = if options.delta == DeltaMode::Never {
        None
    } else {
        let fs = Fs::connect(link)?.with_timeout(options.timeout);
        let found = remote_file(&fs, remote);
        fs.control().close();
        found?
    };
    if let Some(target) = target
        && wants_delta(&transfer, options, target.size)
    {
        return push_delta(link, local, remote, here.len(), verify, options);
    }
    push_whole(link, local, remote, here.len(), verify, options)
}

fn push_whole(
    link: &dyn Open,
    local: &Path,
    remote: &str,
    size: u64,
    verify: Option<Algorithm>,
    options: &SyncOptions,
) -> Result<Moved, Error> {
    let mut request = WriteRequest::replace(remote);
    request.compression.clone_from(&options.compression);
    request.hashes = Some(verify.iter().cloned().collect());
    let writer = Writer::start_with(link, &request, options.timeout, &options.caps)?
        .with_chunk(options.chunk);
    let mut file =
        File::open(local).map_err(|error| Error::local("opening the local file", &error))?;
    let mut hasher = Hasher::new(&verify.iter().cloned().collect::<Vec<_>>())
        .map_err(|error| Error::Compression(error.to_string()))?;
    let mut buffer = vec![0_u8; options.chunk.max(1)];
    loop {
        let count = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Error::local("reading the local file", &error)),
        };
        hasher.update(&buffer[..count]);
        writer.write(&buffer[..count])?;
    }
    let done = writer.finish(options.timeout)?;
    if let Some(algorithm) = &verify {
        let mine = hasher.finish();
        if mine.get(algorithm) != done.hashes.get(algorithm) {
            return Err(Error::Mismatch);
        }
    }
    Ok(Moved {
        size: done.written,
        literal_bytes: done.written.min(size),
        copied_bytes: 0,
        wire_bytes: writer.wire_bytes(),
        delta: false,
        skipped: false,
    })
}

fn push_delta(
    link: &dyn Open,
    local: &Path,
    remote: &str,
    size: u64,
    verify: Option<Algorithm>,
    options: &SyncOptions,
) -> Result<Moved, Error> {
    let blocks = hash_blocks(
        link,
        HashItem::whole(remote),
        std::slice::from_ref(&options.strong),
        &BlockOptions {
            size: options.block_size.map(u64::from),
            weak: None,
            strong_len: options.strong_len.map(|length| length as u64),
        },
    )?;
    let base_size = blocks.result.length;
    let block_size = blocks.signature.spec().block_size;
    // What the new file must hash to: checked by the host before it replaces anything.
    let mut expect = Digests::new();
    if let Some(algorithm) = &verify {
        let mut file =
            File::open(local).map_err(|error| Error::local("opening the local file", &error))?;
        let (digests, _) = digest_reader(
            &mut file,
            std::slice::from_ref(algorithm),
            options.read_size.max(1),
        )
        .map_err(|error| Error::local("reading the local file", &error))?;
        expect = digests;
    }
    let request = WriteRequest {
        mode: WriteMode::Delta {
            block_size: u64::from(block_size),
            base: DeltaBase {
                size: blocks.result.size,
                modified: blocks.result.modified,
                hashes: Digests::new(),
            },
        },
        expect,
        compression: options.compression.clone(),
        hashes: Some(Vec::new()),
        ..WriteRequest::replace(remote)
    };
    let signature_wire = blocks.wire_bytes;
    let mut matcher = Matcher::new(blocks.signature, options.chunk)
        .map_err(|_| bad("the host's signature is not complete"))?;
    let writer = Writer::start_with(link, &request, options.timeout, &options.caps)?
        .with_chunk(options.chunk);
    let mut file =
        File::open(local).map_err(|error| Error::local("opening the local file", &error))?;
    let mut literal = 0_u64;
    let mut copied = 0_u64;
    let mut send = |ops: Vec<Op>| -> Result<(), Error> {
        for op in ops {
            match op {
                Op::Copy { block, count } => {
                    copied += copy_len(base_size, block_size, block, count).unwrap_or(0);
                    writer.copy(block, count)?;
                }
                Op::Literal(bytes) => {
                    literal += bytes.len() as u64;
                    writer.write(&bytes)?;
                }
            }
        }
        Ok(())
    };
    let mut buffer = vec![0_u8; options.chunk.max(1)];
    loop {
        let count = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(Error::local("reading the local file", &error)),
        };
        send(matcher.feed(&buffer[..count]))?;
    }
    send(matcher.finish())?;
    writer.finish(options.timeout)?;
    Ok(Moved {
        size,
        literal_bytes: literal,
        copied_bytes: copied,
        wire_bytes: writer.wire_bytes() + signature_wire,
        delta: true,
        skipped: false,
    })
}

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A temporary file beside the destination: renamed over it when the file is complete, removed when not.
struct Part {
    path: PathBuf,
    out: Option<BufWriter<File>>,
    kept: bool,
}

impl Part {
    fn create(destination: &Path, buffer: usize) -> Result<Self, Error> {
        let name = destination.file_name().map_or_else(
            || "file".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );
        let path = destination.with_file_name(format!(
            ".{name}.bohio-{}-{}.part",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| Error::local("creating the temporary file", &error))?;
        Ok(Self {
            path,
            out: Some(BufWriter::with_capacity(buffer, file)),
            kept: false,
        })
    }

    fn out(&mut self) -> &mut BufWriter<File> {
        self.out
            .as_mut()
            .expect("the part is open until it is put in place")
    }

    /// Flushes, gives the new file the old one's permissions and renames it over `destination`.
    fn put_in_place(
        mut self,
        destination: &Path,
        like: Option<&fs::Metadata>,
    ) -> Result<(), Error> {
        let out = self
            .out
            .take()
            .expect("the part is open until it is put in place");
        let file = out
            .into_inner()
            .map_err(|error| Error::local("writing the temporary file", error.error()))?;
        file.sync_all()
            .map_err(|error| Error::local("writing the temporary file", &error))?;
        drop(file);
        if let Some(meta) = like {
            // The old file's mode, as `replace` on a host keeps it; a failure leaves the default.
            let _ = fs::set_permissions(&self.path, meta.permissions());
        }
        fs::rename(&self.path, destination)
            .map_err(|error| Error::local("replacing the local file", &error))?;
        self.kept = true;
        Ok(())
    }
}

impl Drop for Part {
    fn drop(&mut self) {
        if !self.kept {
            self.out = None;
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Where the bytes of a rebuilt file go: the temporary file, the digest and a count.
struct Tee<'a> {
    out: &'a mut BufWriter<File>,
    hasher: &'a mut Hasher,
    bytes: &'a mut u64,
}

impl Write for Tee<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.out.write_all(bytes)?;
        self.hasher.update(bytes);
        *self.bytes += bytes.len() as u64;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// Fetches `remote` from the host into `local`, replacing it atomically: with a delta against the local copy when
/// `options.delta` and the copy allow, else the whole file. The rebuilt file is checked against a digest the
/// host computed of what it meant to send before it is put in place.
///
/// A delta sends the signature of the local copy (a `bohio-read/1` with `delta`), and the host answers `copy`
/// frames for the blocks the copy has and data for the rest.
///
/// # Errors
///
/// [`Error::Remote`], [`Error::Local`], [`Error::Mismatch`] for a rebuilt file that does not match the host's digest
/// (the local file is untouched), the transport's errors.
pub fn get(
    link: &dyn Open,
    remote: &str,
    local: &Path,
    options: &GetOptions,
) -> Result<Moved, Error> {
    let base = local_file(local)?;
    if base.is_some()
        && let Some(moved) = skip_if_same(link, local, remote, options)?
    {
        return Ok(moved);
    }
    let transfer = transfer_of(link, options)?;
    let verify = verify_with(&transfer, options);
    if let Some(base) = &base
        && wants_delta(&transfer, options, base.len())
    {
        return pull_delta(link, remote, local, base, &transfer, verify, options);
    }
    pull_whole(link, remote, local, base.as_ref(), verify, options)
}

fn read_request(remote: &str, verify: Option<&Algorithm>, options: &SyncOptions) -> ReadRequest {
    let mut request = ReadRequest::whole(remote);
    request.hashes = verify.cloned().into_iter().collect();
    request.compression.clone_from(&options.compression);
    request
}

fn pull_whole(
    link: &dyn Open,
    remote: &str,
    local: &Path,
    like: Option<&fs::Metadata>,
    verify: Option<Algorithm>,
    options: &SyncOptions,
) -> Result<Moved, Error> {
    let request = read_request(remote, verify.as_ref(), options);
    let reader = Reader::start_with(
        link,
        &request,
        options.timeout,
        ReadOptions {
            window_log_max: options.window_log_max,
            ..ReadOptions::default()
        },
    )?;
    let mut part = Part::create(local, options.read_size.max(1))?;
    let mut hasher = Hasher::new(&verify.iter().cloned().collect::<Vec<_>>())
        .map_err(|error| Error::Compression(error.to_string()))?;
    let mut total = 0_u64;
    while let Some(chunk) = reader.next_chunk(options.timeout)? {
        Tee {
            out: part.out(),
            hasher: &mut hasher,
            bytes: &mut total,
        }
        .write_all(&chunk)
        .map_err(|error| Error::local("writing the temporary file", &error))?;
    }
    if let Some(algorithm) = &verify {
        let theirs = reader.digests().unwrap_or_default();
        if theirs.get(algorithm) != hasher.finish().get(algorithm) {
            return Err(Error::Mismatch);
        }
    }
    let wire = reader.wire_bytes();
    part.put_in_place(local, like)?;
    Ok(Moved {
        size: total,
        literal_bytes: total,
        copied_bytes: 0,
        wire_bytes: wire,
        delta: false,
        skipped: false,
    })
}

/// Sends a frame, and when the stream fails says why the host gave up if it said.
fn send_or_why(channel: &dyn Channel, frame: &[u8], sent: &mut u64) -> Result<(), Error> {
    match channel.send_timeout(frame, None) {
        Ok(()) => {
            *sent += frame.len() as u64;
            Ok(())
        }
        Err(failure) => {
            while let Ok(Some(reply)) = channel.receive(Some(Duration::ZERO)) {
                if let Ok(ReadOutput::Error(body)) = ReadOutput::decode(&reply) {
                    return Err(Error::Remote(body));
                }
            }
            Err(failure.into())
        }
    }
}

fn pull_delta(
    link: &dyn Open,
    remote: &str,
    local: &Path,
    base_meta: &fs::Metadata,
    transfer: &TransferInfo,
    verify: Option<Algorithm>,
    options: &SyncOptions,
) -> Result<Moved, Error> {
    let base_size = base_meta.len();
    let (min, max) = (
        u32::try_from(transfer.delta.min_block).unwrap_or(u32::MAX),
        u32::try_from(transfer.delta.max_block).unwrap_or(u32::MAX),
    );
    let block = options
        .block_size
        .unwrap_or_else(|| block_size_for(base_size, min, max));
    let spec = SignatureSpec::new(block, options.strong.clone(), strong_len_of(options)?)
        .map_err(|error| Error::Compression(error.to_string()))?;
    let mut request = read_request(remote, verify.as_ref(), options);
    request.delta = Some(ReadDelta {
        block_size: u64::from(block),
        weak: WEAK_RSYNC.to_owned(),
        strong: spec.strong.clone(),
        strong_len: spec.strong_len as u64,
        base_size,
    });
    let channel = link.open_with(
        protocol::READ,
        &request.to_metadata()?,
        &OpenOptions {
            receive_window: link.bulk_window(),
        },
    )?;
    let channel: &dyn Channel = channel.as_ref();
    let outcome = rebuild(channel, local, base_meta, &spec, verify, options);
    channel.close();
    outcome
}

fn rebuild(
    channel: &dyn Channel,
    local: &Path,
    base_meta: &fs::Metadata,
    spec: &SignatureSpec,
    verify: Option<Algorithm>,
    options: &SyncOptions,
) -> Result<Moved, Error> {
    let mut wire = 0_u64;
    let mut sent = 0_u64;
    // The signature of the local copy goes first, a batch at a time while the file is read.
    let per_batch = (options
        .chunk
        .min(
            channel
                .capabilities()
                .max_send
                .map_or(usize::MAX, |max| max.saturating_sub(1)),
        )
        .saturating_sub(128)
        / (5 + spec.strong_len))
        .max(1);
    let mut base =
        File::open(local).map_err(|error| Error::local("opening the local file", &error))?;
    {
        let mut builder = SignatureBuilder::new(spec.clone())
            .map_err(|error| Error::Compression(error.to_string()))?;
        let mut weak: Vec<u32> = Vec::new();
        let mut strong: Vec<u8> = Vec::new();
        let mut first = 0_u64;
        let mut flush =
            |weak: &mut Vec<u32>, strong: &mut Vec<u8>, force: bool| -> Result<(), Error> {
                if weak.is_empty() || (!force && weak.len() < per_batch) {
                    return Ok(());
                }
                let count = weak.len() as u64;
                let batch = SignatureBatch {
                    first,
                    weak: std::mem::take(weak),
                    strong: std::mem::take(strong),
                };
                first += count;
                send_or_why(channel, &ReadInput::Signature(batch).encode()?, &mut sent)
            };
        let mut buffer = vec![0_u8; options.read_size.max(1)];
        loop {
            let count = match base.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(Error::local("reading the local file", &error)),
            };
            for block in builder.update(&buffer[..count]) {
                weak.push(block.weak);
                strong.extend_from_slice(&block.strong);
                flush(&mut weak, &mut strong, false)?;
            }
        }
        if let Some(block) = builder.finish() {
            weak.push(block.weak);
            strong.extend_from_slice(&block.strong);
        }
        flush(&mut weak, &mut strong, true)?;
        send_or_why(channel, &ReadInput::SignatureEnd.encode()?, &mut sent)?;
    }
    // The host's answer: the header, then copies and data, then the digest.
    let header = loop {
        let Some(frame) = channel.receive(options.timeout)? else {
            return Err(Error::Closed);
        };
        wire += frame.len() as u64;
        match ReadOutput::decode(&frame) {
            Ok(ReadOutput::Header(header)) => break header,
            Ok(ReadOutput::Error(body)) => return Err(Error::Remote(body)),
            Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
            Err(error) => return Err(error.into()),
        }
    };
    if !header.delta {
        return Err(Error::Protocol(bohio_proto::Error::Decode));
    }
    let mut decoder = match header
        .compression
        .as_ref()
        .filter(|used| used.algorithm != Codec::None && used.algorithm.compresses())
    {
        Some(used) => Some(
            Decoder::new(
                used,
                &Bounds {
                    window_log_max: options.window_log_max.unwrap_or(27),
                    max_output: Some(header.length),
                    max_expansion: 0,
                },
            )
            .map_err(|error| Error::Compression(error.to_string()))?,
        ),
        None => None,
    };
    let mut part = Part::create(local, options.read_size.max(1))?;
    let mut hasher = Hasher::new(&verify.iter().cloned().collect::<Vec<_>>())
        .map_err(|error| Error::Compression(error.to_string()))?;
    let mut total = 0_u64;
    let mut copied = 0_u64;
    let theirs: Digests = loop {
        let Some(frame) = channel.receive(options.timeout)? else {
            return Err(Error::Closed);
        };
        wire += frame.len() as u64;
        let protocol_error = || Error::Protocol(bohio_proto::Error::Decode);
        match ReadOutput::decode(&frame) {
            Ok(ReadOutput::Copy(copy)) => {
                let length = copy_len(base_meta.len(), spec.block_size, copy.block, copy.count)
                    .ok_or_else(protocol_error)?;
                if total + length > header.length {
                    return Err(protocol_error());
                }
                copied += length;
                apply(
                    &mut base,
                    &Op::Copy {
                        block: copy.block,
                        count: copy.count,
                    },
                    spec.block_size,
                    &mut Tee {
                        out: part.out(),
                        hasher: &mut hasher,
                        bytes: &mut total,
                    },
                )
                .map_err(|_| protocol_error())?;
            }
            Ok(ReadOutput::Data(bytes)) => {
                // A raw frame ends the compression context.
                if let Some(decoder) = decoder.as_mut() {
                    decoder.reset();
                }
                if total + bytes.len() as u64 > header.length {
                    return Err(protocol_error());
                }
                Tee {
                    out: part.out(),
                    hasher: &mut hasher,
                    bytes: &mut total,
                }
                .write_all(&bytes)
                .map_err(|error| Error::local("writing the temporary file", &error))?;
            }
            Ok(ReadOutput::Compressed(piece)) => {
                let decoder = decoder.as_mut().ok_or_else(protocol_error)?;
                decoder
                    .feed(
                        &piece,
                        &mut Tee {
                            out: part.out(),
                            hasher: &mut hasher,
                            bytes: &mut total,
                        },
                    )
                    .map_err(|error| Error::Compression(error.to_string()))?;
                if total > header.length {
                    return Err(protocol_error());
                }
            }
            Ok(ReadOutput::Done(done)) => break done.hashes,
            Ok(ReadOutput::Error(body)) => return Err(Error::Remote(body)),
            Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
            Err(error) => return Err(error.into()),
        }
    };
    if total != header.length {
        return Err(Error::Closed);
    }
    if let Some(algorithm) = &verify
        && theirs.get(algorithm) != hasher.finish().get(algorithm)
    {
        return Err(Error::Mismatch);
    }
    drop(base);
    part.put_in_place(local, Some(base_meta))?;
    Ok(Moved {
        size: total,
        literal_bytes: total - copied,
        copied_bytes: copied,
        wire_bytes: wire + sent,
        delta: true,
        skipped: false,
    })
}
