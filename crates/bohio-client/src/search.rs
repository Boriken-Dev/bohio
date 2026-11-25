//! The lines of files on a host that match a pattern, found on the host: `bohio-search/1`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use bohio_proto::protocol;
use bohio_proto::search::{
    SearchDone, SearchItemError, SearchMatch, SearchOutput, SearchRequest, SearchScope,
};
use coqui_link::mux::LinkStream;

use crate::{Error, Open};

/// What a search matches and how far it reads. The host's `[search]` settings bound every field that is a bound.
#[derive(Debug, Clone, Default)]
pub struct SearchOptions {
    /// How far below a root to go: `0` is the root alone (a file), `None` the whole tree.
    pub depth: Option<u64>,
    /// Only files whose path or name matches one of these globs are read (directories are still entered).
    pub include: Vec<String>,
    /// Entries whose path or name matches one of these globs are left out, and a directory's whole subtree.
    pub exclude: Vec<String>,
    /// Search names that start with a dot too.
    pub hidden: bool,
    /// Take the pattern as plain text, not a regular expression.
    pub literal: bool,
    /// Ignore case.
    pub case_insensitive: bool,
    /// Lines of context before and after each match.
    pub context: u64,
    /// The most matches to send; `None` asks for the host's `[search] max_matches`.
    pub max_matches: Option<u64>,
    /// The most files to read; `None` asks for the host's `[search] max_files`.
    pub max_files: Option<u64>,
}

/// One matching line, with the file it is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Which root: its position among the roots given to [`search`], from `0`.
    pub root: u64,
    /// The file's path below the root with `/`; empty when the root is the file.
    pub path: String,
    /// The file's size in bytes.
    pub size: u64,
    /// The line: its number from `1`, its text (cut at the host's `max_line_bytes`, `truncated` says so) and the
    /// context lines around it.
    pub hit: SearchMatch,
}

/// One thing a search found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchItem {
    /// A matching line.
    Match(Box<Found>),
    /// A directory, file or root the host could not read (the search went on).
    Failure(SearchItemError),
}

/// What a finished search gathered: [`SearchIter::finish`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResults {
    /// The matching lines, in the order the host found them.
    pub matches: Vec<Found>,
    /// What could not be read.
    pub failures: Vec<SearchItemError>,
    /// The host's closing counts: files read, matches, what was skipped and whether a bound ended the search.
    pub done: SearchDone,
}

/// The matches of a search, as they arrive. Dropping it closes the stream, which stops the host.
pub struct SearchIter {
    channel: Arc<dyn LinkStream>,
    pending: VecDeque<SearchItem>,
    file: Option<(u64, String, u64)>,
    done: Option<SearchDone>,
    ended: bool,
    timeout: Option<Duration>,
}

impl core::fmt::Debug for SearchIter {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SearchIter")
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

/// Starts a search of `roots` on the host for `pattern` (a regular expression, or text with
/// [`SearchOptions::literal`]).
///
/// The host refuses at the first frame, so a refusal is the iterator's first item: `bad_request` for an empty pattern,
/// one that does not compile or is too large, a glob that does not parse or a context over `max_context`; `denied`
/// without `fs.read`; `busy` at `[search] max_searches`. A root outside the peer's roots, and files that cannot be read,
/// come as [`SearchItem::Failure`]. Links are never followed.
///
/// # Errors
///
/// [`Error::Closed`] or whatever `link` reports when the stream cannot be opened.
pub fn search(
    link: &dyn Open,
    roots: &[&str],
    pattern: &str,
    options: &SearchOptions,
) -> Result<SearchIter, Error> {
    search_with(
        link,
        roots,
        pattern,
        options,
        Some(Duration::from_secs(300)),
    )
}

/// As [`search`], waiting at most `timeout` for each frame (`None` waits without limit).
///
/// # Errors
///
/// As [`search`].
pub fn search_with(
    link: &dyn Open,
    roots: &[&str],
    pattern: &str,
    options: &SearchOptions,
    timeout: Option<Duration>,
) -> Result<SearchIter, Error> {
    let request = SearchRequest {
        source: None,
        walk: SearchScope {
            roots: roots.iter().map(|root| (*root).to_owned()).collect(),
            depth: options.depth,
            include: options.include.clone(),
            exclude: options.exclude.clone(),
        },
        hidden: options.hidden,
        pattern: pattern.to_owned(),
        literal: options.literal,
        case_insensitive: options.case_insensitive,
        context: options.context,
        max_matches: options.max_matches,
        max_files: options.max_files,
    };
    let channel: Arc<dyn LinkStream> =
        Arc::from(link.open(protocol::SEARCH, &request.to_metadata()?)?);
    Ok(SearchIter {
        channel,
        pending: VecDeque::new(),
        file: None,
        done: None,
        ended: false,
        timeout,
    })
}

impl SearchIter {
    /// The host's closing counts, once the search has ended.
    pub const fn done(&self) -> Option<SearchDone> {
        self.done
    }

    /// Reads the rest of the search.
    ///
    /// # Errors
    ///
    /// The first error the stream gave: [`Error::Remote`] for a refusal, [`Error::Closed`] when the host ended it
    /// without a closing frame.
    pub fn finish(mut self) -> Result<SearchResults, Error> {
        let mut matches = Vec::new();
        let mut failures = Vec::new();
        for item in self.by_ref() {
            match item? {
                SearchItem::Match(found) => matches.push(*found),
                SearchItem::Failure(failure) => failures.push(failure),
            }
        }
        let done = self.done.ok_or(Error::Closed)?;
        Ok(SearchResults {
            matches,
            failures,
            done,
        })
    }

    fn next_frame(&mut self) -> Result<(), Error> {
        let Some(frame) = self.channel.receive(self.timeout)? else {
            return Err(Error::Closed);
        };
        match SearchOutput::decode(&frame) {
            Ok(SearchOutput::File(file)) => self.file = Some((file.root, file.path, file.size)),
            Ok(SearchOutput::Match(hit)) => {
                let (root, path, size) = self
                    .file
                    .clone()
                    .ok_or(Error::Protocol(bohio_proto::Error::Decode))?;
                self.pending.push_back(SearchItem::Match(Box::new(Found {
                    root,
                    path,
                    size,
                    hit,
                })));
            }
            Ok(SearchOutput::ItemError(error)) => {
                self.pending.push_back(SearchItem::Failure(error));
            }
            Ok(SearchOutput::Done(done)) => {
                self.done = Some(done);
                self.ended = true;
            }
            Ok(SearchOutput::Error(body)) => {
                self.ended = true;
                return Err(Error::Remote(body));
            }
            Ok(_) | Err(bohio_proto::Error::Tag(_)) => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }
}

impl Iterator for SearchIter {
    type Item = Result<SearchItem, Error>;

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

impl Drop for SearchIter {
    fn drop(&mut self) {
        // Closing tells a host that is still searching to stop.
        self.channel.close();
    }
}
