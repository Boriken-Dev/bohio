//! `bohio-search/1`: the lines of files under the peer's roots that match a pattern, so the matches and not the
//! tree cross the link.

use std::collections::{BTreeSet, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use bohio_proto::search::{
    SearchDone, SearchFile, SearchItemError, SearchMatch, SearchOutput, SearchRequest, Skipped,
};
use bohio_proto::{ErrorBody, ErrorCode};
use cap_std::fs::Dir;
use coqui_transport::Channel;
use globset::GlobSet;
use regex::{Regex, RegexBuilder};

use super::bundle::grant_now;
use super::ops::{self, Roots};
use super::walk::globs;
use super::{Context, authorize, io_error};
use crate::config::SearchConfig;
use crate::limit::Slots;
use crate::policy::Permission;
use crate::stream::{message_limit, send_waiting};

/// What the search engine needs besides the stream.
#[derive(Debug, Clone, Copy)]
pub struct SearchContext<'a> {
    /// The filesystem engines' context: the peer's grant (`current` is read before each root), `[filesystem]`.
    pub files: Context<'a>,
    /// The `[search]` settings.
    pub search: &'a SearchConfig,
    /// The bound on concurrent search streams, shared by every stream.
    pub slots: &'a Slots,
}

/// Bytes a match frame holds besides its lines: an estimate that keeps a frame inside the stream's message bound.
const FRAME_OVERHEAD: usize = 256;

fn encode_output(output: &SearchOutput) -> Vec<u8> {
    output.encode().unwrap_or_else(|_| vec![0x05])
}

/// Serves one `bohio-search/1` stream whose open carried `metadata`, until the roots are searched or a bound is
/// reached. Returns how many matches were sent.
///
/// Frames out: for each file with a match a `file` frame and then its `match` frames (the lines of context
/// inside them), item errors for what could not be read (the search goes on), then `done`. Each root is decided by the
/// peer's grant as it is when the root is reached. Links are never followed: a link is neither read nor entered, so
/// nothing outside the roots is. A file is skipped, and counted in `done`, when it is over `[search] max_file_bytes`
/// or holds a NUL in its first `binary_probe` bytes. The pattern is never logged.
///
/// # Errors
///
/// The [`ErrorBody`] sent as the stream's `error` frame: `bad_request` for an undecodable request, no root, an empty
/// pattern, a pattern that does not compile or compiles past `[search] regex_size_limit`, a glob that does not parse, a
/// zero bound or a context over `max_context`; `denied` without `fs.read`; `busy` at `max_searches`. A root outside
/// the peer's roots is an item error, not an error.
pub fn serve_search(
    channel: Arc<dyn Channel>,
    metadata: &[u8],
    context: &SearchContext<'_>,
) -> Result<u64, ErrorBody> {
    let poll = context.files.config.poll_interval.0;
    match run(&channel, metadata, context) {
        Ok(matches) => Ok(matches),
        Err(error) => {
            // Best effort: a stream with no room for one more frame right now gets none.
            let frame = encode_output(&SearchOutput::Error(error.clone()));
            let _ = send_waiting(channel.as_ref(), &frame, poll, poll);
            Err(error)
        }
    }
}

fn bad(message: &'static str) -> ErrorBody {
    ErrorBody::new(ErrorCode::BadRequest, message)
}

fn run(
    channel: &Arc<dyn Channel>,
    metadata: &[u8],
    context: &SearchContext<'_>,
) -> Result<u64, ErrorBody> {
    let files = &context.files;
    let settings = context.search;
    let request =
        SearchRequest::from_metadata(metadata).map_err(|_| bad("the request cannot be read"))?;
    authorize(files, &[Permission::FsRead])?;
    if request.walk.roots.is_empty() {
        return Err(bad("a search needs at least one root"));
    }
    if request.pattern.is_empty() {
        return Err(bad("the pattern is empty"));
    }
    if request.context > settings.max_context {
        return Err(bad("the context is more lines than [search] max_context"));
    }
    let max_matches = bound(request.max_matches, settings.max_matches)?;
    let max_files = bound(request.max_files, settings.max_files)?;
    let source = if request.literal {
        regex::escape(&request.pattern)
    } else {
        request.pattern.clone()
    };
    let pattern = RegexBuilder::new(&source)
        .case_insensitive(request.case_insensitive)
        .size_limit(settings.regex_size_limit)
        .dfa_size_limit(settings.regex_size_limit)
        .build()
        .map_err(|_| bad("the pattern is not a regular expression, or is larger than [search] regex_size_limit"))?;
    let include = globs(&request.walk.include)?;
    let exclude = globs(&request.walk.exclude)?;
    let _permit = context.slots.acquire().ok_or_else(|| {
        ErrorBody::new(
            ErrorCode::Busy,
            "the host is running as many searches as it allows",
        )
    })?;
    // A match frame holds up to `2 * context + 1` lines: the cap on a line keeps it inside what the stream carries.
    let lines = usize::try_from(request.context)
        .unwrap_or(usize::MAX)
        .saturating_mul(2)
        .saturating_add(1);
    let line_cap = settings
        .max_line_bytes
        .min((message_limit(channel.as_ref()).saturating_sub(FRAME_OVERHEAD) / lines).max(16));
    let mut searcher = Searcher {
        channel: channel.as_ref(),
        context,
        request: &request,
        pattern: &pattern,
        include: &include,
        exclude: &exclude,
        max_matches,
        max_files,
        line_cap,
        started: Instant::now(),
        read: 0,
        sent: 0,
        skipped: Skipped::default(),
        truncated: false,
        failed: false,
    };
    for (root, text) in request.walk.roots.iter().enumerate() {
        if searcher.stopped() {
            break;
        }
        if let Err(body) = searcher.search_root(root as u64, text) {
            searcher.item_error(root as u64, None, &body);
        }
    }
    if searcher.failed {
        return Err(ErrorBody::new(
            ErrorCode::Io,
            "the stream ended before the search was done",
        ));
    }
    let done = SearchOutput::Done(SearchDone {
        files: searcher.read,
        matches: searcher.sent,
        skipped: searcher.skipped,
        truncated: searcher.truncated,
    });
    let poll = files.config.poll_interval.0;
    if !send_waiting(
        channel.as_ref(),
        &encode_output(&done),
        poll,
        files.config.idle_timeout.0,
    ) {
        return Err(ErrorBody::new(
            ErrorCode::Io,
            "the stream ended before the search was done",
        ));
    }
    Ok(searcher.sent)
}

/// What a request's bound comes to under the host's: a request may ask for less; zero is meaningless.
fn bound(asked: Option<u64>, host: u64) -> Result<u64, ErrorBody> {
    match asked {
        None => Ok(host),
        Some(0) => Err(bad("a bound of zero matches nothing")),
        Some(asked) => Ok(asked.min(host)),
    }
}

struct Searcher<'a> {
    channel: &'a dyn Channel,
    context: &'a SearchContext<'a>,
    request: &'a SearchRequest,
    pattern: &'a Regex,
    include: &'a GlobSet,
    exclude: &'a GlobSet,
    max_matches: u64,
    max_files: u64,
    line_cap: usize,
    started: Instant,
    /// Files read.
    read: u64,
    /// Matches sent.
    sent: u64,
    skipped: Skipped,
    truncated: bool,
    /// The stream cannot go on (the client left).
    failed: bool,
}

/// One directory being listed: its path, its sorted names and how far through them the search is.
struct Level {
    relative: PathBuf,
    prefix: String,
    names: Vec<String>,
    next: usize,
    depth: u64,
}

/// What reading a file came to.
enum Loaded {
    Text(Vec<u8>),
    Binary,
    Large,
}

/// A match waiting for the lines after it.
struct Pending {
    line: u64,
    text: String,
    truncated: bool,
    before: Vec<String>,
    after: Vec<String>,
}

fn dot(relative: &Path) -> &Path {
    if relative.as_os_str().is_empty() {
        Path::new(".")
    } else {
        relative
    }
}

impl Searcher<'_> {
    /// Whether the search must stop: a bound was reached, the stream failed or the deadline passed.
    fn stopped(&mut self) -> bool {
        if self.failed || self.truncated {
            return true;
        }
        if self.started.elapsed() >= self.context.search.deadline.0 || self.channel.is_closed() {
            self.truncated = true;
            return true;
        }
        false
    }

    fn send(&mut self, output: &SearchOutput) {
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
        let error = SearchItemError {
            root,
            path: path.map(str::to_owned),
            code: body.code,
            message: body.message.clone(),
        };
        self.send(&SearchOutput::ItemError(error));
    }

    fn search_root(&mut self, root: u64, text: &str) -> Result<(), ErrorBody> {
        let grant = grant_now(&self.context.files)?;
        let files = Context {
            grant: &grant,
            ..self.context.files
        };
        authorize(&files, &[Permission::FsRead])?;
        let roots = Roots::new(grant.fs_roots());
        let (dir, relative) = roots.resolve(text)?;
        let here = dot(&relative).to_path_buf();
        let meta = dir
            .symlink_metadata(&here)
            .map_err(|error| io_error(&error))?;
        if meta.is_symlink() {
            return Err(ErrorBody::new(
                ErrorCode::Unsupported,
                "a search does not follow a link",
            ));
        }
        if meta.is_file() {
            self.file(root, &dir, &here, "", meta.len());
            return Ok(());
        }
        if !meta.is_dir() {
            return Ok(());
        }
        let mut stack: Vec<Level> = Vec::new();
        if self.request.walk.depth != Some(0)
            && let Some(level) = self.enter(root, &dir, &here, "", 1)
        {
            stack.push(level);
        }
        while let Some(level) = stack.last_mut() {
            if self.stopped() {
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
            if (name.starts_with('.') && !self.request.hidden)
                || self.exclude.is_match(&text)
                || self.exclude.is_match(&name)
            {
                continue;
            }
            let meta = match dir.symlink_metadata(&child) {
                Ok(meta) => meta,
                Err(error) => {
                    self.item_error(root, Some(&text), &io_error(&error));
                    continue;
                }
            };
            if meta.is_dir() {
                if self.request.walk.depth.is_none_or(|limit| depth < limit)
                    && let Some(next) = self.enter(root, &dir, &child, &text, depth + 1)
                {
                    stack.push(next);
                }
            } else if meta.is_file()
                && (self.include.is_empty()
                    || self.include.is_match(&text)
                    || self.include.is_match(&name))
            {
                self.file(root, &dir, &child, &text, meta.len());
            }
            // A link, a device or a socket is left alone.
        }
        Ok(())
    }

    /// The sorted names of the directory at `relative`.
    fn enter(
        &mut self,
        root: u64,
        dir: &Dir,
        relative: &Path,
        text: &str,
        depth: u64,
    ) -> Option<Level> {
        let shown = if text.is_empty() { None } else { Some(text) };
        let read = match super::native::read_dir(dir, relative) {
            Ok(read) => read,
            Err(error) => {
                self.item_error(root, shown, &io_error(&error));
                return None;
            }
        };
        let mut names: BTreeSet<String> = BTreeSet::new();
        for entry in read {
            let Ok(entry) = entry else { continue };
            if let Ok(name) = entry.file_name().into_string() {
                names.insert(name);
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

    /// Reads the file at `relative` (`text` below its root) and sends its matches.
    fn file(&mut self, root: u64, dir: &Dir, relative: &Path, text: &str, size: u64) {
        let settings = self.context.search;
        if self.read >= self.max_files {
            self.truncated = true;
            return;
        }
        if size > settings.max_file_bytes {
            self.skipped.large += 1;
            return;
        }
        self.read += 1;
        let shown = Some(text).filter(|text| !text.is_empty());
        match self.load(dir, relative) {
            Ok(Loaded::Text(content)) => self.scan(root, text, &content),
            Ok(Loaded::Binary) => self.skipped.binary += 1,
            Ok(Loaded::Large) => self.skipped.large += 1,
            Err(body) => {
                self.skipped.unreadable += 1;
                self.item_error(root, shown, &body);
            }
        }
    }

    /// The file's bytes, unless it is binary or grew past `max_file_bytes` while it was read.
    fn load(&self, dir: &Dir, relative: &Path) -> Result<Loaded, ErrorBody> {
        let settings = self.context.search;
        let (mut file, _) = ops::open_regular_in(dir, relative)?;
        let mut content = Vec::new();
        let probe = u64::try_from(settings.binary_probe).unwrap_or(u64::MAX);
        file.by_ref()
            .take(probe)
            .read_to_end(&mut content)
            .map_err(|error| io_error(&error))?;
        if content.contains(&0) {
            return Ok(Loaded::Binary);
        }
        file.by_ref()
            .take(
                settings
                    .max_file_bytes
                    .saturating_sub(content.len() as u64)
                    .saturating_add(1),
            )
            .read_to_end(&mut content)
            .map_err(|error| io_error(&error))?;
        if content.len() as u64 > settings.max_file_bytes {
            return Ok(Loaded::Large);
        }
        Ok(Loaded::Text(content))
    }

    fn scan(&mut self, root: u64, text: &str, content: &[u8]) {
        let context = usize::try_from(self.request.context).unwrap_or(usize::MAX);
        let mut before: VecDeque<String> = VecDeque::new();
        let mut pending: VecDeque<Pending> = VecDeque::new();
        let mut announced = false;
        let size = content.len() as u64;
        let mut number = 0_u64;
        let mut lines = content.split(|byte| *byte == b'\n').peekable();
        while let Some(raw) = lines.next() {
            if lines.peek().is_none() && raw.is_empty() {
                break;
            }
            number += 1;
            if number.is_multiple_of(4096) && self.stopped() {
                break;
            }
            let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
            let line = String::from_utf8_lossy(raw);
            let (shown, cut) = cut(&line, self.line_cap);
            // The lines this one follows complete the matches waiting for context.
            for waiting in &mut pending {
                if waiting.after.len() < context {
                    waiting.after.push(shown.to_owned());
                }
            }
            while pending
                .front()
                .is_some_and(|waiting| waiting.after.len() >= context)
            {
                let waiting = pending.pop_front().unwrap_or_else(|| unreachable!());
                self.emit(root, text, size, &mut announced, waiting);
            }
            if self.truncated && pending.is_empty() {
                break;
            }
            if self.failed {
                return;
            }
            if !self.truncated && self.pattern.is_match(&line) {
                let found = Pending {
                    line: number,
                    text: shown.to_owned(),
                    truncated: cut,
                    before: before.iter().cloned().collect(),
                    after: Vec::new(),
                };
                self.sent += 1;
                if context == 0 {
                    self.emit(root, text, size, &mut announced, found);
                } else {
                    pending.push_back(found);
                }
                if self.sent >= self.max_matches {
                    // The lines after the last match are still read, for its context.
                    self.truncated = true;
                    if pending.is_empty() {
                        break;
                    }
                }
            }
            if context > 0 {
                before.push_back(shown.to_owned());
                if before.len() > context {
                    before.pop_front();
                }
            }
        }
        for waiting in pending {
            self.emit(root, text, size, &mut announced, waiting);
        }
    }

    fn emit(&mut self, root: u64, text: &str, size: u64, announced: &mut bool, found: Pending) {
        if !*announced {
            *announced = true;
            self.send(&SearchOutput::File(SearchFile {
                root,
                path: text.to_owned(),
                size,
            }));
        }
        self.send(&SearchOutput::Match(SearchMatch {
            line: found.line,
            text: found.text,
            before: found.before,
            after: found.after,
            truncated: found.truncated,
        }));
    }
}

/// `line` cut to at most `cap` bytes at a character boundary, and whether it was.
fn cut(line: &str, cap: usize) -> (&str, bool) {
    if line.len() <= cap {
        return (line, false);
    }
    let mut end = cap;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    (&line[..end], true)
}
