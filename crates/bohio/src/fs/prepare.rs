//! Prepared reads: a range compressed in full into a file before any of it is sent, so a slow link
//! pays for the best ratio once and a broken transfer resumes from a compressed offset.
//!
//! A prepared file is `<token>.bin` (the compressed stream) and `<token>.meta` (who made it and what
//! it holds), both in `[compression] prepare_dir`, named by a random 128-bit token the client must
//! present from the same peer. The `.meta` is written last, so a file without one is a preparation
//! that did not finish. Files older than `prepare_ttl` are removed by the next prepared read and
//! when the node starts.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use bohio_proto::compress::Compression;
use bohio_proto::hash::Digests;
use bohio_proto::{ErrorBody, ErrorCode, decode, encode};
use bohio_transfer::compress::Encoder;
use bohio_transfer::hash::Hasher;
use serde::{Deserialize, Serialize};

use super::io_error;

/// What a prepared file records about itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Meta {
    /// The peer it was made for; only that peer may read it.
    pub(crate) peer: String,
    /// The path the peer asked for.
    pub(crate) path: String,
    /// The file's size when it was read.
    pub(crate) size: u64,
    /// The bytes compressed (the range's length).
    pub(crate) length: u64,
    /// What the host used.
    pub(crate) compression: Compression,
    /// The size of the compressed stream.
    pub(crate) compressed_length: u64,
    /// The digests of the range the request asked for.
    pub(crate) hashes: Digests,
}

/// Removes the files in `dir` older than `ttl`; a missing directory is nothing to remove.
pub fn sweep(dir: &Path, ttl: Duration) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|made| SystemTime::now().duration_since(made).ok())
            .is_some_and(|age| age >= ttl);
        let ours = entry.path().extension().is_some_and(|extension| {
            extension == "bin" || extension == "meta" || extension == "part"
        });
        if old && ours {
            let _ = fs::remove_file(entry.path());
        }
    }
}

pub(crate) fn token() -> Result<String, ErrorBody> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| {
        ErrorBody::new(
            ErrorCode::Internal,
            "the host cannot make an unguessable name",
        )
    })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Whether `text` is the shape of a token (so a name a peer sends cannot lead out of the directory).
pub(crate) fn well_formed(text: &str) -> bool {
    text.len() == 32
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn bin(dir: &Path, token: &str) -> PathBuf {
    dir.join(format!("{token}.bin"))
}

fn meta(dir: &Path, token: &str) -> PathBuf {
    dir.join(format!("{token}.meta"))
}

/// What a preparation needs to know.
pub(crate) struct Job<'a> {
    pub(crate) dir: &'a Path,
    pub(crate) max_bytes: u64,
    pub(crate) peer: &'a str,
    pub(crate) path: &'a str,
    pub(crate) size: u64,
    pub(crate) length: u64,
    pub(crate) chunk: usize,
    /// How often `progress` is called at least, while work goes on.
    pub(crate) every: Duration,
}

/// A prepared file: its name, what it records and the compressed stream, open.
pub(crate) struct Prepared {
    pub(crate) token: String,
    pub(crate) meta: Meta,
    pub(crate) file: File,
}

/// Removes the files it names unless it is disarmed.
struct Cleanup(Vec<PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

/// Compresses `length` bytes of `source` (positioned at the range's start) into a new prepared file.
///
/// `progress` is told how many bytes are compressed, at least every `job.every`, and returns `false`
/// when the stream is gone, which ends the work and removes what was made. The digests of the bytes
/// read are taken in the same pass.
///
/// # Errors
///
/// `busy` when the compressed stream passes `max_bytes`, `io` for a file that shrinks or a disk that
/// fails, `unsupported` when the codec cannot start.
pub(crate) fn make(
    job: &Job<'_>,
    source: &mut impl Read,
    mut encoder: Encoder,
    mut hasher: Hasher,
    progress: &mut dyn FnMut(u64) -> bool,
) -> Result<Prepared, ErrorBody> {
    fs::create_dir_all(job.dir).map_err(|error| io_error(&error))?;
    let token = token()?;
    let part = job.dir.join(format!("{token}.part"));
    let mut cleanup = Cleanup(vec![part.clone(), bin(job.dir, &token)]);
    let mut out = File::create(&part).map_err(|error| io_error(&error))?;
    let used = encoder.used().clone();
    let mut buffer = vec![0_u8; job.chunk.max(1)];
    let (mut remaining, mut done, mut stored) = (job.length, 0_u64, 0_u64);
    let mut last = Instant::now();
    let too_large = || {
        ErrorBody::new(
            ErrorCode::Busy,
            "the compressed result is larger than this host keeps",
        )
    };
    while remaining > 0 {
        let want = usize::try_from(remaining).map_or(buffer.len(), |left| left.min(buffer.len()));
        let count = match source.read(&mut buffer[..want]) {
            Ok(0) => {
                return Err(ErrorBody::new(
                    ErrorCode::Io,
                    "the file shrank while it was read",
                ));
            }
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(io_error(&error)),
        };
        hasher.update(&buffer[..count]);
        let piece = encoder
            .piece(&buffer[..count], false)
            .map_err(|error| ErrorBody::new(ErrorCode::Internal, error.to_string()))?;
        stored += piece.len() as u64;
        if stored > job.max_bytes {
            return Err(too_large());
        }
        out.write_all(&piece).map_err(|error| io_error(&error))?;
        remaining -= count as u64;
        done += count as u64;
        if last.elapsed() >= job.every {
            last = Instant::now();
            if !progress(done) {
                return Err(ErrorBody::new(
                    ErrorCode::Io,
                    "the stream ended while the read was prepared",
                ));
            }
        }
    }
    let tail = encoder
        .finish()
        .map_err(|error| ErrorBody::new(ErrorCode::Internal, error.to_string()))?;
    stored += tail.len() as u64;
    if stored > job.max_bytes {
        return Err(too_large());
    }
    out.write_all(&tail).map_err(|error| io_error(&error))?;
    out.flush().map_err(|error| io_error(&error))?;
    drop(out);
    let record = Meta {
        peer: job.peer.to_owned(),
        path: job.path.to_owned(),
        size: job.size,
        length: job.length,
        compression: used,
        compressed_length: stored,
        hashes: hasher.finish(),
    };
    let bytes = encode(&record)
        .map_err(|_| ErrorBody::new(ErrorCode::Internal, "the record cannot be written"))?;
    cleanup.0.push(meta(job.dir, &token));
    fs::rename(&part, bin(job.dir, &token)).map_err(|error| io_error(&error))?;
    fs::write(meta(job.dir, &token), bytes).map_err(|error| io_error(&error))?;
    // Made: only the removal of an expired file is left for later.
    cleanup.0.clear();
    let file = File::open(bin(job.dir, &token)).map_err(|error| io_error(&error))?;
    Ok(Prepared {
        token,
        meta: record,
        file,
    })
}

/// Opens the prepared file `token` for `peer`, to resume from a compressed offset.
///
/// # Errors
///
/// `not_found` for a token that is not one, belongs to nothing, belongs to another peer or has
/// expired (the same answer for each, so a token cannot be probed).
pub(crate) fn open(
    dir: &Path,
    token: &str,
    peer: &str,
    ttl: Duration,
) -> Result<Prepared, ErrorBody> {
    let gone = || {
        ErrorBody::new(
            ErrorCode::NotFound,
            "there is no such prepared read for this peer",
        )
    };
    if !well_formed(token) {
        return Err(gone());
    }
    let record_path = meta(dir, token);
    let made = fs::metadata(&record_path)
        .and_then(|meta| meta.modified())
        .map_err(|_| gone())?;
    if SystemTime::now()
        .duration_since(made)
        .is_ok_and(|age| age >= ttl)
    {
        let _ = fs::remove_file(&record_path);
        let _ = fs::remove_file(bin(dir, token));
        return Err(gone());
    }
    let bytes = fs::read(&record_path).map_err(|_| gone())?;
    let record: Meta = decode(&bytes).map_err(|_| gone())?;
    if record.peer != peer {
        return Err(gone());
    }
    let file = File::open(bin(dir, token)).map_err(|_| gone())?;
    Ok(Prepared {
        token: token.to_owned(),
        meta: record,
        file,
    })
}
