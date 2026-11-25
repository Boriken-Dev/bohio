//! `bohio-walk/1`: the entries of whole trees on one stream, so listing a tree costs a round trip for all of it
//! and not one for each directory and page.

use std::collections::{BTreeSet, HashSet};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use super::vfs::{self, Dir};
use bohio_proto::fs::Metadata;
use bohio_proto::hash::Algorithm;
use bohio_proto::walk::{
    WalkDone, WalkEntries, WalkEntry, WalkHeader, WalkInput, WalkItemError, WalkOutput, WalkRequest,
};
use bohio_proto::{ErrorBody, ErrorCode, encode};
use bohio_transfer::compress::{Encoder, Piece};
use bohio_transfer::hash::Hasher;
use coqui_transport::{Channel, Failure};
use globset::{Glob, GlobSet, GlobSetBuilder};

use super::bundle::grant_now;
use super::compress::Settings;
use super::hash::{Pace, allowed};
use super::ops::{self, Roots};
use super::{Context, authorize, io_error};
use crate::config::WalkConfig;
use crate::limit::Slots;
use crate::policy::Permission;
use crate::stream::{chunk_size, send_waiting};

/// What the walk engine needs besides the stream.
#[derive(Debug, Clone, Copy)]
pub struct WalkContext<'a> {
    /// The filesystem engines' context: the peer's grant (`current` is read before each root), `[filesystem]`,
    /// `[hash]` and `[compression]`.
    pub files: Context<'a>,
    /// The `[walk]` settings.
    pub walk: &'a WalkConfig,
    /// The bound on concurrent walk streams, shared by every stream.
    pub slots: &'a Slots,
}

/// Bytes of an entry's encoding besides its path: an estimate that keeps a frame inside the stream's message
/// bound.
const ENTRY_OVERHEAD: usize = 320;

/// Bytes a digest adds to an entry, hex text and its key, at most.
const DIGEST_OVERHEAD: usize = 150;

fn encode_output(output: &WalkOutput) -> Vec<u8> {
    output.encode().unwrap_or_else(|_| vec![0x05])
}

/// Serves one `bohio-walk/1` stream whose open carried `metadata`, until the client ends its roots.
///
/// Frames in: one `root` per tree, then `end`. Frames out: `header`, then for each root in order batches of
/// `entries` (the root itself first, with the path empty; a directory's names in the order of their UTF-8 bytes,
/// each directory entered right after its own entry), item errors for what could not be read (the walk goes on),
/// then `done`. Each root is decided by the peer's grant as it is when the root is reached, so a grant narrowed
/// while the stream is open refuses the next root. Links are described and, with `follow`, entered only where
/// they stay inside the root, once for each directory (by device and inode). A stream that gets no root for
/// `idle_timeout`, or whose client closes it, ends. Returns how many entries were sent.
///
/// # Errors
///
/// The [`ErrorBody`] sent as the stream's `error` frame: `bad_request` for an undecodable request or a glob that
/// does not parse, `unsupported` for an algorithm or compression the host does not offer, `denied` before any
/// root, and `busy` at `[walk] max_streams`. Past `[walk] max_entries` the walk ends with an item error and
/// `done`, not an error.
pub fn serve_walk(
    channel: Arc<dyn Channel>,
    metadata: &[u8],
    context: &WalkContext<'_>,
) -> Result<u64, ErrorBody> {
    let poll = context.files.config.poll_interval.0;
    match run(&channel, metadata, context) {
        Ok(entries) => Ok(entries),
        Err(error) => {
            // Best effort: a stream with no room for one more frame right now gets none.
            let frame = encode_output(&WalkOutput::Error(error.clone()));
            let _ = send_waiting(channel.as_ref(), &frame, poll, poll);
            Err(error)
        }
    }
}

pub(super) fn globs(patterns: &[String]) -> Result<GlobSet, ErrorBody> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).map_err(|_| {
            ErrorBody::new(ErrorCode::BadRequest, format!("`{pattern}` is not a glob"))
        })?;
        builder.add(glob);
    }
    builder
        .build()
        .map_err(|_| ErrorBody::new(ErrorCode::BadRequest, "the globs cannot be used"))
}

fn run(
    channel: &Arc<dyn Channel>,
    metadata: &[u8],
    context: &WalkContext<'_>,
) -> Result<u64, ErrorBody> {
    let files = &context.files;
    let poll = files.config.poll_interval.0;
    let idle = files.config.idle_timeout.0;
    let request = WalkRequest::from_metadata(metadata)
        .map_err(|_| ErrorBody::new(ErrorCode::BadRequest, "the request cannot be read"))?;
    authorize(files, &[Permission::FsRead])?;
    let include = globs(&request.include)?;
    let exclude = globs(&request.exclude)?;
    allowed(files, &request.hashes)?;
    Hasher::new(&request.hashes)
        .map_err(|error| ErrorBody::new(ErrorCode::Unsupported, error.to_string()))?;
    let settings = Settings::new(files.compression);
    let resolved = settings.resolve(request.compression.as_ref())?;
    let encoder = resolved
        .as_ref()
        .map(|compression| Encoder::new(compression, settings.caps()))
        .transpose()
        .map_err(|error| ErrorBody::new(ErrorCode::Unsupported, error.to_string()))?;
    let _permit = context.slots.acquire().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Busy,
            "the host is running as many walk streams as it allows",
        )
    })?;
    let ended = || ErrorBody::new(ErrorCode::Io, "the stream ended before the roots were done");
    let header = WalkOutput::Header(WalkHeader {
        compression: encoder.as_ref().map(|encoder| encoder.used().clone()),
    });
    if !send_waiting(channel.as_ref(), &encode_output(&header), poll, idle) {
        return Err(ended());
    }
    let mut walker = Walker {
        channel: channel.as_ref(),
        context,
        request: &request,
        include: &include,
        exclude: &exclude,
        encoder,
        batch: Vec::new(),
        batch_bytes: 0,
        limit: chunk_size(channel.as_ref(), files.config.chunk_size)
            .saturating_sub(1024)
            .max(1),
        sent: 0,
        skipped: 0,
        exhausted: false,
        pace: Pace::new(files.hash.rate, poll),
        failed: false,
    };
    let mut root = 0_u64;
    let mut last_progress = Instant::now();
    loop {
        match channel.receive(Some(poll)) {
            Ok(Some(frame)) => match WalkInput::decode(&frame) {
                Ok(WalkInput::Root(found)) => {
                    walker.walk_root(root, &found.path);
                    if walker.failed {
                        return Err(ended());
                    }
                    root += 1;
                    last_progress = Instant::now();
                }
                Ok(WalkInput::End) => break,
                // A frame this version does not know is skipped.
                Ok(_) | Err(_) => {}
            },
            Err(Failure::Timeout) => {
                if channel.is_closed() {
                    return Err(ended());
                }
                if last_progress.elapsed() >= idle {
                    return Err(ErrorBody::new(
                        ErrorCode::Io,
                        "the client sent no root for too long",
                    ));
                }
            }
            Ok(None) | Err(_) => return Err(ended()),
        }
    }
    let done = WalkOutput::Done(WalkDone {
        entries: walker.sent,
        skipped: walker.skipped,
    });
    if !send_waiting(channel.as_ref(), &encode_output(&done), poll, idle) {
        return Err(ended());
    }
    Ok(walker.sent)
}

struct Walker<'a> {
    channel: &'a dyn Channel,
    context: &'a WalkContext<'a>,
    request: &'a WalkRequest,
    include: &'a GlobSet,
    exclude: &'a GlobSet,
    encoder: Option<Encoder>,
    batch: Vec<WalkEntry>,
    batch_bytes: usize,
    /// The most bytes of estimated entries in one frame.
    limit: usize,
    sent: u64,
    skipped: u64,
    /// `[walk] max_entries` was reached: nothing more is walked.
    exhausted: bool,
    pace: Pace,
    /// The stream cannot go on (the client left).
    failed: bool,
}

/// One directory being listed: its path, its sorted names and how far through them the walk is.
struct Level {
    relative: PathBuf,
    prefix: String,
    names: Vec<String>,
    next: usize,
    depth: u64,
}

impl Walker<'_> {
    fn send(&mut self, output: &WalkOutput) {
        let files = &self.context.files;
        if !send_waiting(
            self.channel,
            &encode_output(output),
            files.config.poll_interval.0,
            files.config.idle_timeout.0,
        ) {
            self.failed = true;
        }
    }

    fn item_error(&mut self, root: u64, path: Option<&str>, body: &ErrorBody) {
        let error = WalkItemError {
            root,
            path: path.map(str::to_owned),
            code: body.code,
            message: body.message.clone(),
        };
        self.send(&WalkOutput::ItemError(error));
    }

    /// Sends the entries gathered for `root`.
    fn flush(&mut self, root: u64) {
        if self.batch.is_empty() {
            return;
        }
        let entries = WalkEntries {
            root,
            entries: std::mem::take(&mut self.batch),
        };
        self.batch_bytes = 0;
        let output = match self.encoder.as_mut() {
            Some(encoder) => match encode(&entries)
                .ok()
                .and_then(|body| encoder.frame(&body).ok().map(|piece| (body, piece)))
            {
                Some((_, Piece::Compressed(piece))) => WalkOutput::Compressed(piece),
                _ => WalkOutput::Entries(entries),
            },
            None => WalkOutput::Entries(entries),
        };
        self.send(&output);
    }

    fn push(&mut self, root: u64, entry: WalkEntry) {
        self.batch_bytes += entry.path.len()
            + ENTRY_OVERHEAD
            + entry.target.as_ref().map_or(0, String::len)
            + entry
                .hashes
                .as_ref()
                .map_or(0, |hashes| hashes.len() * DIGEST_OVERHEAD);
        self.batch.push(entry);
        self.sent += 1;
        if self.batch_bytes >= self.limit {
            self.flush(root);
        }
    }

    /// Whether another entry may be sent; at the limit, says so once and stops the walk.
    fn room(&mut self, root: u64) -> bool {
        let max = self.context.walk.max_entries;
        if max != 0 && self.sent >= max {
            if !self.exhausted {
                self.exhausted = true;
                self.flush(root);
                let body = ErrorBody::new(
                    ErrorCode::Busy,
                    "the walk reached [walk] max_entries and stopped",
                );
                self.item_error(root, None, &body);
            }
            return false;
        }
        true
    }

    fn walk_root(&mut self, root: u64, text: &str) {
        if self.exhausted {
            let body = ErrorBody::new(
                ErrorCode::Busy,
                "the walk reached [walk] max_entries and stopped",
            );
            self.item_error(root, None, &body);
            return;
        }
        if let Err(body) = self.walk_root_checked(root, text) {
            self.flush(root);
            self.item_error(root, None, &body);
        }
        self.flush(root);
    }

    fn walk_root_checked(&mut self, root: u64, text: &str) -> Result<(), ErrorBody> {
        let grant = grant_now(&self.context.files)?;
        let files = Context {
            grant: &grant,
            ..self.context.files
        };
        authorize(&files, &[Permission::FsRead])?;
        let roots = Roots::new(grant.fs_roots(), files.source);
        let (dir, relative) = roots.resolve(text)?;
        if !self.room(root) {
            return Ok(());
        }
        let here = if relative.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            relative.clone()
        };
        let meta = dir
            .symlink_metadata(&here)
            .map_err(|error| io_error(&error))?;
        let mut visited: HashSet<(u64, u64)> = HashSet::new();
        let entry = self.entry(&dir, &here, "", &meta, root);
        self.push(root, entry);
        let mut stack: Vec<Level> = Vec::new();
        if self.request.depth != Some(0)
            && let Some(level) = self.enter(root, &dir, &here, "", 1, &meta, &mut visited)
        {
            stack.push(level);
        }
        while let Some(level) = stack.last_mut() {
            if self.failed {
                return Ok(());
            }
            let Some(name) = level.names.get(level.next).cloned() else {
                stack.pop();
                continue;
            };
            level.next += 1;
            let child = level.relative.join(&name);
            let text = if level.prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{name}", level.prefix)
            };
            let depth = level.depth;
            if self.exclude.is_match(&text) || self.exclude.is_match(&name) {
                continue;
            }
            let meta = match dir.symlink_metadata(&child) {
                Ok(meta) => meta,
                Err(error) => {
                    self.item_error(root, Some(&text), &io_error(&error));
                    continue;
                }
            };
            let listed = self.include.is_empty()
                || self.include.is_match(&text)
                || self.include.is_match(&name);
            if listed {
                if !self.room(root) {
                    return Ok(());
                }
                let entry = self.entry(&dir, &child, &text, &meta, root);
                self.push(root, entry);
            }
            if self.request.depth.is_none_or(|limit| depth < limit)
                && let Some(next) =
                    self.enter(root, &dir, &child, &text, depth + 1, &meta, &mut visited)
            {
                stack.push(next);
            }
        }
        Ok(())
    }

    /// The entry of `relative` (under `dir`), named `text` below its root.
    fn entry(
        &mut self,
        dir: &Dir,
        relative: &std::path::Path,
        text: &str,
        meta: &vfs::Metadata,
        root: u64,
    ) -> WalkEntry {
        let described: Metadata = ops::describe(meta);
        let target = if meta.is_symlink() {
            dir.read_link(relative).ok()
        } else {
            None
        };
        let hashes = if meta.is_file() && !self.request.hashes.is_empty() {
            match self.hash(dir, relative) {
                Ok(digests) => Some(digests),
                Err(body) => {
                    self.item_error(root, Some(text), &body);
                    None
                }
            }
        } else {
            None
        };
        WalkEntry {
            path: text.to_owned(),
            metadata: described,
            target,
            hashes,
        }
    }

    fn hash(
        &mut self,
        dir: &Dir,
        relative: &std::path::Path,
    ) -> Result<bohio_proto::hash::Digests, ErrorBody> {
        let (mut file, _) = ops::open_regular_in(dir, relative)?;
        let algorithms: &[Algorithm] = &self.request.hashes;
        let mut hasher = Hasher::new(algorithms)
            .map_err(|error| ErrorBody::new(ErrorCode::Unsupported, error.to_string()))?;
        let files = &self.context.files;
        let mut buffer = vec![0_u8; self.pace.piece(files.hash.read_size)];
        loop {
            if self.channel.is_closed() {
                return Err(ErrorBody::new(
                    ErrorCode::Io,
                    "the stream ended before the walk was done",
                ));
            }
            match file.read(&mut buffer) {
                Ok(0) => return Ok(hasher.finish()),
                Ok(count) => {
                    hasher.update(&buffer[..count]);
                    self.pace.spent(self.channel, count as u64);
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(io_error(&error)),
            }
        }
    }

    /// The names of the directory `meta` describes at `relative`, when it is one and may be entered: itself, or a
    /// link to one with `follow` on a host that tells directories apart.
    #[allow(clippy::too_many_arguments)]
    fn enter(
        &mut self,
        root: u64,
        dir: &Dir,
        relative: &std::path::Path,
        text: &str,
        depth: u64,
        meta: &vfs::Metadata,
        visited: &mut HashSet<(u64, u64)>,
    ) -> Option<Level> {
        let shown = if text.is_empty() { None } else { Some(text) };
        let target = if meta.is_dir() {
            meta.clone()
        } else if meta.is_symlink() && self.request.follow {
            match dir.metadata(relative) {
                Ok(target) if target.is_dir() => target,
                Ok(_) => return None,
                Err(error) => {
                    self.item_error(root, shown, &io_error(&error));
                    return None;
                }
            }
        } else {
            return None;
        };
        if self.request.follow {
            match identity(&target) {
                Some(id) => {
                    if !visited.insert(id) {
                        return None;
                    }
                }
                None => {
                    if meta.is_symlink() {
                        let body = ErrorBody::new(
                            ErrorCode::Unsupported,
                            "this host cannot tell directories apart, so it does not enter a link to one",
                        );
                        self.item_error(root, shown, &body);
                        return None;
                    }
                }
            }
        }
        let remaining = {
            let max = self.context.walk.max_entries;
            if max == 0 {
                usize::MAX
            } else {
                usize::try_from(max.saturating_sub(self.sent)).unwrap_or(usize::MAX)
            }
        };
        let read = match dir.read_dir(relative) {
            Ok(read) => read,
            Err(error) => {
                self.item_error(root, shown, &io_error(&error));
                return None;
            }
        };
        // Only the smallest names that can still be sent are kept, so a huge directory costs one pass and a
        // bounded amount of memory.
        let mut names: BTreeSet<String> = BTreeSet::new();
        for entry in read {
            let Ok(entry) = entry else { continue };
            let Ok(name) = entry.file_name().into_string() else {
                self.skipped += 1;
                continue;
            };
            names.insert(name);
            if names.len() > remaining.saturating_add(1) {
                names.pop_last();
            }
        }
        Some(Level {
            relative: relative.to_path_buf(),
            prefix: text.to_owned(),
            names: names.into_iter().collect(),
            next: 0,
            depth,
        })
    }
}

/// The device and inode of `meta`, where the host reports them.
fn identity(meta: &vfs::Metadata) -> Option<(u64, u64)> {
    let described = ops::describe(meta);
    Some((described.device?, described.inode?))
}
