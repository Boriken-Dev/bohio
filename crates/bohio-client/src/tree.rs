//! Keeping a local tree and a tree of a host the same: walk both, compare without moving a byte, then send only
//! what differs, the small files in bundles and the large ones by delta.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bohio_proto::fs::{FileKind, ReadItem, Timestamp};
use bohio_proto::hash::{Algorithm, Digests};
use bohio_proto::{ErrorBody, ErrorCode};
use bohio_transfer::hash::digest_reader;

use crate::bundle::{BundleFile, BundleOptions, Fetch, put_many, read_bundle};
use crate::fs::Fs;
use crate::sync::{Compare, SyncOptions, compare_file, get, put};
use crate::walk::{WalkItem, WalkOptions, walk_with};
use crate::{Error, Open};

/// Which side is the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Direction {
    /// The local tree is copied to the host.
    #[default]
    Push,
    /// The host's tree is copied here.
    Pull,
}

/// What [`sync_tree`] does.
#[derive(Debug, Clone)]
pub struct TreeOptions {
    /// Which side is the source. Default push.
    pub direction: Direction,
    /// How a file on both sides is told to be the same: [`Compare::Meta`] (size and modification time, nothing is
    /// read), [`Compare::Hash`] (the host hashes during the walk, this side reads its files) or [`Compare::Blocks`]
    /// (a block signature of each, file by file). Default `Meta`.
    pub compare: Compare,
    /// Remove from the destination what the source does not have. Default false.
    pub delete: bool,
    /// Change nothing: [`TreeReport::actions`] says what would be done. Default false.
    pub dry_run: bool,
    /// The largest file moved in a bundle; larger ones go one by one, by delta when the destination has an older
    /// copy. Default 1 MiB.
    pub bundle_max: u64,
    /// The most files in one bundle. At least 1. Default 1000.
    pub bundle_items: usize,
    /// How many bundles and large files move at once, each on a stream of its own. At least 1. Default 4.
    pub parallel: usize,
    /// How far apart two modification times may be and still be the same time: a destination that keeps them coarsely
    /// (FAT keeps two seconds) is not copied again for it. Default one microsecond.
    pub modify_window: Duration,
    /// How large files are sent and received (delta, compression, timeouts). Its `compare` is ignored: the tree
    /// decides which files move.
    pub sync: SyncOptions,
    /// How bundles are compressed and timed. Its `hashes` are ignored.
    pub bundle: BundleOptions,
}

impl Default for TreeOptions {
    fn default() -> Self {
        Self {
            direction: Direction::Push,
            compare: Compare::Meta,
            delete: false,
            dry_run: false,
            bundle_max: 1 << 20,
            bundle_items: 1000,
            parallel: 4,
            modify_window: Duration::from_micros(1),
            sync: SyncOptions::default(),
            bundle: BundleOptions::default(),
        }
    }
}

/// What one step of a sync does (or, in a dry run, would do).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// A directory is made on the destination.
    MakeDirectory,
    /// A file the destination lacks is copied.
    Create,
    /// A file that differs is copied over the destination's.
    Update,
    /// A file or directory the source lacks is removed from the destination.
    Delete,
}

/// One planned or done step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    /// What is done.
    pub step: Step,
    /// The path below the roots, with `/`.
    pub path: String,
    /// The file's size in bytes (`0` for a directory).
    pub size: u64,
}

/// What [`sync_tree`] found and did.
#[derive(Debug, Clone, Default)]
pub struct TreeReport {
    /// Every step, in the order they were decided.
    pub actions: Vec<Action>,
    /// Files on both sides that were the same.
    pub unchanged: u64,
    /// Files copied in bundles.
    pub bundled: u64,
    /// How many bundle streams were used.
    pub bundles: u64,
    /// Large files copied (by delta or whole).
    pub singles: u64,
    /// Of those, the ones copied by delta.
    pub deltas: u64,
    /// Files and directories removed from the destination.
    pub deleted: u64,
    /// Links and other entries that are not files or directories, which a sync does not copy.
    pub skipped: u64,
    /// The bytes of file content that moved, before compression.
    pub bytes: u64,
    /// The bytes of frames that travelled for the files (not the walk), both ways.
    pub wire_bytes: u64,
    /// The entries the host's walk listed, and the bytes of its frames.
    pub walk_entries: u64,
    /// See `walk_entries`.
    pub walk_wire_bytes: u64,
    /// What went wrong for one path (the sync went on), with the path.
    pub failures: Vec<(String, String)>,
}

impl TreeReport {
    /// Whether every step worked.
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }
}

/// What one side holds at a path.
#[derive(Debug, Clone)]
struct Node {
    kind: FileKind,
    size: u64,
    modified: Option<Timestamp>,
    #[cfg_attr(not(unix), allow(dead_code))]
    mode: Option<u32>,
    hashes: Option<Digests>,
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

fn system_time(stamp: Timestamp) -> Option<SystemTime> {
    if stamp.secs >= 0 {
        UNIX_EPOCH.checked_add(Duration::new(stamp.secs.unsigned_abs(), stamp.nanos))
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::from_secs(stamp.secs.unsigned_abs()))
            .and_then(|before| before.checked_add(Duration::from_nanos(u64::from(stamp.nanos))))
    }
}

fn nanos(stamp: Timestamp) -> i128 {
    i128::from(stamp.secs) * 1_000_000_000 + i128::from(stamp.nanos)
}

fn join(root: &str, relative: &str) -> String {
    if relative.is_empty() {
        return root.to_owned();
    }
    format!("{}/{relative}", root.trim_end_matches('/'))
}

/// Why `relative`, a path the host named below its root, cannot be a path below a local one; `windows` applies
/// the rules of Windows names, which differ from a Unix name's.
fn refuse_name(relative: &str, windows: bool) -> Option<&'static str> {
    for part in relative.split('/') {
        if part.is_empty() {
            return Some("an empty part, a leading or a doubled slash");
        }
        if part == "." || part == ".." {
            return Some("a `.` or `..` part");
        }
        if part.contains('\0') {
            return Some("a NUL in a name");
        }
        if !windows {
            continue;
        }
        if part.contains(['\\', ':']) {
            return Some("a backslash, a drive or a stream in a name");
        }
        if part.contains(['<', '>', '"', '|', '?', '*']) || part.chars().any(char::is_control) {
            return Some("a character a Windows name may not hold");
        }
        if part.ends_with(' ') || part.ends_with('.') {
            return Some("a name ending in a space or a dot");
        }
        let stem = part.split('.').next().unwrap_or_default().trim_end();
        let stem = stem.to_ascii_uppercase();
        let device = matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|digit| matches!(digit.as_bytes(), [b'1'..=b'9']));
        if device {
            return Some("a device name");
        }
    }
    None
}

fn refusal(relative: &str, why: &str) -> Error {
    Error::Local(format!(
        "the host named a path that cannot be below the local tree ({why}): {relative:?}"
    ))
}

/// The local path of a path the host named below its root: relative, with no empty, `.` or `..` part, no part a
/// local name cannot be, and resolving below `root` even through a link already there. There is no way to allow
/// more: a path outside the tree cannot be what a sync is asked to copy. `links` is false for a tree that is only
/// read (a push), whose names come from its own listing.
fn local_path(root: &Path, relative: &str, links: bool) -> Result<PathBuf, Error> {
    if let Some(why) = refuse_name(relative, cfg!(windows)) {
        return Err(refusal(relative, why));
    }
    if !links {
        let mut full = root.to_path_buf();
        full.extend(relative.split('/'));
        return Ok(full);
    }
    let mut path = root.to_path_buf();
    // The deepest part that exists, resolved through every link on the way, must stay below the root.
    let mut deepest = None;
    for part in relative.split('/') {
        path.push(part);
        match fs::symlink_metadata(&path) {
            Ok(_) => deepest = Some(path.clone()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => break,
            Err(error) => return Err(Error::local("looking at a local path", &error)),
        }
    }
    if let Some(deepest) = deepest {
        let escape = || refusal(relative, "it leads out of the local tree through a link");
        let inside = fs::canonicalize(root)
            .and_then(|root| fs::canonicalize(&deepest).map(|found| found.starts_with(root)))
            .map_err(|_| escape())?;
        if !inside {
            return Err(escape());
        }
    }
    let mut full = root.to_path_buf();
    full.extend(relative.split('/'));
    Ok(full)
}

/// Every path of the host's listing, checked before anything is read or written: a host that names one it may
/// not ends the sync with an error naming each.
fn check_listing(listing: &BTreeMap<String, Node>, local: Option<&Path>) -> Result<(), Error> {
    let mut refused = Vec::new();
    for relative in listing.keys() {
        let result = match local {
            Some(root) => local_path(root, relative, true).map(drop),
            None => refuse_name(relative, cfg!(windows))
                .map_or(Ok(()), |why| Err(refusal(relative, why))),
        };
        if let Err(error) = result {
            tracing::warn!(path = relative.as_str(), %error, "refused a path the host listed");
            refused.push(error.to_string());
        }
    }
    if refused.is_empty() {
        return Ok(());
    }
    let first = refused.remove(0);
    Err(Error::Local(if refused.is_empty() {
        first
    } else {
        format!("{first} (and {} more)", refused.len())
    }))
}

fn failed(report: &mut TreeReport, path: &str, error: &Error) {
    report.failures.push((path.to_owned(), error.to_string()));
}

/// Walks the local tree: every file and directory below `root` (links and other kinds are counted as skipped).
fn scan_local(
    root: &Path,
    report: &mut TreeReport,
    hash: Option<&Algorithm>,
    read_size: usize,
) -> Result<BTreeMap<String, Node>, Error> {
    let top =
        fs::metadata(root).map_err(|error| Error::local("looking at the local tree", &error))?;
    if !top.is_dir() {
        return Err(Error::Local("the local tree is not a directory".to_owned()));
    }
    let mut found = BTreeMap::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    while let Some((directory, prefix)) = stack.pop() {
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) => {
                report.failures.push((
                    prefix.clone(),
                    format!("reading the local directory: {error}"),
                ));
                continue;
            }
        };
        for entry in entries {
            let Ok(entry) = entry else { continue };
            let Ok(name) = entry.file_name().into_string() else {
                report.skipped += 1;
                continue;
            };
            let relative = if prefix.is_empty() {
                name
            } else {
                format!("{prefix}/{name}")
            };
            let Ok(meta) = fs::symlink_metadata(entry.path()) else {
                continue;
            };
            let kind = if meta.is_file() {
                FileKind::File
            } else if meta.is_dir() {
                FileKind::Directory
            } else {
                report.skipped += 1;
                continue;
            };
            #[cfg(unix)]
            let mode = {
                use std::os::unix::fs::PermissionsExt;
                Some(meta.permissions().mode() & 0o7777)
            };
            #[cfg(not(unix))]
            let mode = None;
            let hashes = match (hash, kind) {
                (Some(algorithm), FileKind::File) => File::open(entry.path())
                    .and_then(|mut file| {
                        digest_reader(&mut file, std::slice::from_ref(algorithm), read_size.max(1))
                    })
                    .ok()
                    .map(|(digests, _)| digests),
                _ => None,
            };
            if kind == FileKind::Directory {
                stack.push((entry.path(), relative.clone()));
            }
            found.insert(
                relative,
                Node {
                    kind,
                    size: if kind == FileKind::File {
                        meta.len()
                    } else {
                        0
                    },
                    modified: meta.modified().ok().map(timestamp),
                    mode,
                    hashes,
                },
            );
        }
    }
    Ok(found)
}

/// Walks the host's tree. `Ok(None)` when the root does not exist.
fn scan_remote(
    link: &dyn Open,
    remote: &str,
    report: &mut TreeReport,
    hash: Option<&Algorithm>,
    options: &TreeOptions,
) -> Result<Option<BTreeMap<String, Node>>, Error> {
    let walk_options = WalkOptions {
        hashes: hash.cloned().into_iter().collect(),
        compression: options.bundle.compression.clone(),
        ..WalkOptions::default()
    };
    let mut iter = walk_with(link, &[remote], &walk_options, options.sync.timeout)?;
    let mut found = BTreeMap::new();
    let mut missing = false;
    for item in iter.by_ref() {
        match item? {
            WalkItem::Entry { entry, .. } => {
                let entry = *entry;
                if entry.path.is_empty() {
                    if entry.metadata.kind != FileKind::Directory {
                        return Err(Error::Remote(ErrorBody::new(
                            ErrorCode::BadRequest,
                            "the root on the host is not a directory",
                        )));
                    }
                    continue;
                }
                report.walk_entries += 1;
                let kind = entry.metadata.kind;
                found.insert(
                    entry.path,
                    Node {
                        kind,
                        size: if kind == FileKind::File {
                            entry.metadata.size
                        } else {
                            0
                        },
                        modified: entry.metadata.modified,
                        mode: entry.metadata.mode,
                        hashes: entry.hashes,
                    },
                );
            }
            WalkItem::Failure(failure) => {
                if failure.path.is_none() && failure.code == ErrorCode::NotFound {
                    missing = true;
                } else {
                    report.failures.push((
                        failure.path.clone().unwrap_or_default(),
                        format!("{:?}: {}", failure.code, failure.message),
                    ));
                }
            }
        }
    }
    report.walk_wire_bytes += iter.wire_bytes();
    check_listing(&found, None)?;
    Ok((!missing).then_some(found))
}

/// What a sync has to do about one path that is on the source.
enum Decision {
    Same,
    Send(Step),
    Conflict,
}

struct Context<'a> {
    link: &'a dyn Open,
    options: &'a TreeOptions,
    local: &'a Path,
    remote: &'a str,
}

fn same_time(a: Option<Timestamp>, b: Option<Timestamp>, window: Duration) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            let limit = i128::try_from(window.as_nanos()).unwrap_or(i128::MAX);
            (nanos(a) - nanos(b)).abs() <= limit
        }
        _ => false,
    }
}

impl Context<'_> {
    /// The local path of `relative`, checked: see [`local_path`].
    fn local_path(&self, relative: &str) -> Result<PathBuf, Error> {
        local_path(
            self.local,
            relative,
            self.options.direction == Direction::Pull,
        )
    }

    /// Whether the file `relative` is the same on both sides.
    fn compare(&self, relative: &str, source: &Node, destination: &Node) -> Result<bool, Error> {
        if source.size != destination.size {
            return Ok(false);
        }
        match &self.options.compare {
            Compare::Meta => Ok(same_time(
                source.modified,
                destination.modified,
                self.options.modify_window,
            )),
            Compare::Hash(algorithm) => Ok(match (&source.hashes, &destination.hashes) {
                (Some(a), Some(b)) => {
                    a.contains_key(algorithm) && a.get(algorithm) == b.get(algorithm)
                }
                _ => false,
            }),
            Compare::Blocks => {
                let verdict = compare_file(
                    self.link,
                    &self.local_path(relative)?,
                    &join(self.remote, relative),
                    Compare::Blocks,
                    &self.options.sync,
                )?;
                Ok(verdict == crate::sync::Verdict::Same)
            }
        }
    }

    fn decide(
        &self,
        relative: &str,
        source: &Node,
        destination: Option<&Node>,
    ) -> Result<Decision, Error> {
        let Some(destination) = destination else {
            return Ok(Decision::Send(Step::Create));
        };
        if destination.kind != source.kind {
            return Ok(Decision::Conflict);
        }
        if source.kind == FileKind::Directory {
            return Ok(Decision::Same);
        }
        Ok(if self.compare(relative, source, destination)? {
            Decision::Same
        } else {
            Decision::Send(Step::Update)
        })
    }
}

/// One unit of work: a bundle of small files or a large file.
enum Job {
    Bundle(Vec<String>),
    Single(String),
}

/// Makes the tree on the destination the same as the source's.
///
/// Both trees are walked (the host's in one stream), every file on both sides is compared by `options.compare`, and
/// only what differs moves: files up to `options.bundle_max` in bundles of `bundle_items` (one stream, one
/// compression context each), larger ones one by one, by delta against the destination's older copy when it has
/// one. A pushed or pulled file takes the source's modification time, so the next sync by size and time finds it
/// unchanged. With `delete` the destination loses what the source lacks (after the copies); with `dry_run` nothing
/// changes and [`TreeReport::actions`] says what would. Links and other special entries are skipped. A path that
/// fails is recorded in [`TreeReport::failures`] and the sync goes on.
///
/// # Errors
///
/// [`Error::Local`] for a local root that is not a directory, or for a host that lists a path that cannot be below
/// the local tree (absolute, with a `..` or empty part, a name the local system cannot have, or reaching out of the
/// tree through a link already there): nothing is read or written and there is no option to allow it. [`Error::Remote`] for a host that refuses the walk (or,
/// when pulling, has no such root), and the transport's errors. What goes wrong for one file is not an error.
pub fn sync_tree(
    link: &dyn Open,
    local: &Path,
    remote: &str,
    options: &TreeOptions,
) -> Result<TreeReport, Error> {
    let mut report = TreeReport::default();
    let hash = match &options.compare {
        Compare::Hash(algorithm) => Some(algorithm),
        _ => None,
    };
    let (source, destination) = match options.direction {
        Direction::Push => {
            let here = scan_local(local, &mut report, hash, options.sync.read_size)?;
            let there = scan_remote(link, remote, &mut report, hash, options)?;
            (here, there)
        }
        Direction::Pull => {
            let there =
                scan_remote(link, remote, &mut report, hash, options)?.ok_or_else(|| {
                    Error::Remote(ErrorBody::new(
                        ErrorCode::NotFound,
                        "the tree on the host does not exist",
                    ))
                })?;
            let here = if local.exists() {
                scan_local(local, &mut report, hash, options.sync.read_size)?
            } else {
                BTreeMap::new()
            };
            (there, Some(here))
        }
    };
    if options.direction == Direction::Pull {
        // Before anything is read or written: no name of the host's may leave the local tree.
        check_listing(&source, Some(local))?;
    }
    // The destination's root is missing when it is the host's and the walk found nothing.
    let destination_root_exists = destination.is_some();
    let destination = destination.unwrap_or_default();
    let context = Context {
        link,
        options,
        local,
        remote,
    };

    let mut directories: Vec<String> = Vec::new();
    let mut files: BTreeMap<String, Step> = BTreeMap::new();
    let mut removals: Vec<String> = Vec::new();
    for (relative, node) in &source {
        match context.decide(relative, node, destination.get(relative)) {
            Ok(Decision::Same) => {
                if node.kind == FileKind::File {
                    report.unchanged += 1;
                }
            }
            Ok(Decision::Send(step)) => match node.kind {
                FileKind::Directory => {
                    directories.push(relative.clone());
                    report.actions.push(Action {
                        step: Step::MakeDirectory,
                        path: relative.clone(),
                        size: 0,
                    });
                }
                _ => {
                    files.insert(relative.clone(), step);
                    report.actions.push(Action {
                        step,
                        path: relative.clone(),
                        size: node.size,
                    });
                }
            },
            Ok(Decision::Conflict) => {
                if options.delete {
                    removals.push(relative.clone());
                    if node.kind == FileKind::Directory {
                        directories.push(relative.clone());
                        report.actions.push(Action {
                            step: Step::MakeDirectory,
                            path: relative.clone(),
                            size: 0,
                        });
                    } else {
                        files.insert(relative.clone(), Step::Update);
                        report.actions.push(Action {
                            step: Step::Update,
                            path: relative.clone(),
                            size: node.size,
                        });
                    }
                } else {
                    report.failures.push((
                        relative.clone(),
                        "a file and a directory are in each other's way (set delete to replace it)"
                            .to_owned(),
                    ));
                }
            }
            Err(error) => failed(&mut report, relative, &error),
        }
    }
    // What the destination has that the source lacks, outermost first: a removed directory takes what is in it.
    let mut doomed: Vec<String> = Vec::new();
    if options.delete {
        for (relative, node) in &destination {
            if source.contains_key(relative) {
                continue;
            }
            // A directory that goes, or is replaced by a file, takes what is in it.
            if doomed
                .iter()
                .chain(removals.iter())
                .any(|gone| relative.starts_with(&format!("{gone}/")))
            {
                continue;
            }
            doomed.push(relative.clone());
            report.actions.push(Action {
                step: Step::Delete,
                path: relative.clone(),
                size: node.size,
            });
        }
    }
    if options.dry_run {
        return Ok(report);
    }

    let (bundled, singles): (Vec<String>, Vec<String>) = files.keys().cloned().partition(|path| {
        source
            .get(path)
            .is_some_and(|node| node.size <= options.bundle_max)
    });
    apply(
        &context,
        &source,
        &destination,
        destination_root_exists,
        &removals,
        &directories,
        bundled,
        singles,
        &mut report,
    )?;
    if options.delete {
        remove_extras(&context, &doomed, &mut report)?;
    }
    Ok(report)
}

/// Makes the destination's directories, then moves the files.
#[allow(clippy::too_many_arguments)]
fn apply(
    context: &Context<'_>,
    source: &BTreeMap<String, Node>,
    destination: &BTreeMap<String, Node>,
    destination_root_exists: bool,
    removals: &[String],
    directories: &[String],
    bundled: Vec<String>,
    singles: Vec<String>,
    report: &mut TreeReport,
) -> Result<(), Error> {
    let options = context.options;
    let push = options.direction == Direction::Push;
    let fs = Fs::connect(context.link)?.with_timeout(options.sync.timeout);
    if !push {
        fs::create_dir_all(context.local)
            .map_err(|error| Error::local("making the local tree", &error))?;
    }
    // A path in the way is removed first.
    for relative in removals {
        let result = if push {
            fs.remove(&join(context.remote, relative), true)
        } else {
            context
                .local_path(relative)
                .and_then(|path| remove_local(&path))
        };
        if let Err(error) = result {
            failed(report, relative, &error);
        }
    }
    // A bundle's file makes the directories above it; the others are made one by one.
    let under_a_bundled_file: BTreeSet<&str> = bundled
        .iter()
        .flat_map(|path| {
            path.match_indices('/')
                .map(|(at, _)| &path[..at])
                .collect::<Vec<_>>()
        })
        .collect();
    let make = |relative: &str| -> Result<(), Error> {
        if push {
            fs.mkdir(&join(context.remote, relative), true)
        } else {
            fs::create_dir_all(context.local_path(relative)?)
                .map_err(|error| Error::local("making a local directory", &error))
        }
    };
    if !destination_root_exists
        && push
        && let Err(error) = fs.mkdir(context.remote, true)
    {
        failed(report, "", &error);
    }
    for relative in directories {
        if push && under_a_bundled_file.contains(relative.as_str()) {
            continue;
        }
        if let Err(error) = make(relative) {
            failed(report, relative, &error);
        }
    }
    // The parents of the single files must exist before their streams open.
    let mut parents: BTreeSet<&str> = BTreeSet::new();
    for path in &singles {
        if let Some((parent, _)) = path.rsplit_once('/')
            && !destination
                .get(parent)
                .is_some_and(|node| node.kind == FileKind::Directory)
            && !directories.iter().any(|made| made == parent)
        {
            parents.insert(parent);
        }
    }
    for parent in parents {
        if let Err(error) = make(parent) {
            failed(report, parent, &error);
        }
    }
    if !push {
        // A pulled bundle's files are written through paths, so their parents are made up front.
        for path in &bundled {
            if let Some((parent, _)) = path.rsplit_once('/') {
                let made = context.local_path(parent).and_then(|directory| {
                    fs::create_dir_all(directory)
                        .map_err(|error| Error::local("making a local directory", &error))
                });
                if let Err(error) = made {
                    failed(report, parent, &error);
                }
            }
        }
    }
    drop(fs);

    let mut jobs: Vec<Job> = Vec::new();
    for chunk in bundled.chunks(options.bundle_items.max(1)) {
        jobs.push(Job::Bundle(chunk.to_vec()));
    }
    jobs.extend(singles.into_iter().map(Job::Single));
    let next = AtomicUsize::new(0);
    let shared = Mutex::new(&mut *report);
    let workers = options.parallel.max(1).min(jobs.len().max(1));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let at = next.fetch_add(1, Ordering::Relaxed);
                    let Some(job) = jobs.get(at) else { return };
                    let mut local_report = TreeReport::default();
                    match job {
                        Job::Bundle(paths) => {
                            run_bundle(context, source, paths, &mut local_report);
                        }
                        Job::Single(path) => run_single(context, source, path, &mut local_report),
                    }
                    let mut guard = shared
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    merge(&mut guard, local_report);
                }
            });
        }
    });
    Ok(())
}

fn merge(into: &mut TreeReport, from: TreeReport) {
    into.bundled += from.bundled;
    into.bundles += from.bundles;
    into.singles += from.singles;
    into.deltas += from.deltas;
    into.bytes += from.bytes;
    into.wire_bytes += from.wire_bytes;
    into.failures.extend(from.failures);
}

fn remove_local(path: &Path) -> Result<(), Error> {
    let meta = fs::symlink_metadata(path).map_err(|error| Error::local("removing", &error))?;
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .map_err(|error| Error::local("removing", &error))
}

fn remove_extras(
    context: &Context<'_>,
    doomed: &[String],
    report: &mut TreeReport,
) -> Result<(), Error> {
    let push = context.options.direction == Direction::Push;
    let fs = if push {
        Some(Fs::connect(context.link)?.with_timeout(context.options.sync.timeout))
    } else {
        None
    };
    for relative in doomed {
        let result = match &fs {
            Some(fs) => fs.remove(&join(context.remote, relative), true),
            None => context
                .local_path(relative)
                .and_then(|path| remove_local(&path)),
        };
        match result {
            Ok(()) => report.deleted += 1,
            Err(error) => failed(report, relative, &error),
        }
    }
    Ok(())
}

/// A local file opened when its bytes are first asked for and closed at its end, so a bundle of thousands holds one
/// descriptor at a time.
struct Lazy {
    path: PathBuf,
    file: Option<File>,
    done: bool,
}

impl Read for Lazy {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        if self.file.is_none() {
            self.file = Some(File::open(&self.path)?);
        }
        let count = self.file.as_mut().map_or(Ok(0), |file| file.read(out))?;
        if count == 0 {
            self.done = true;
            self.file = None;
        }
        Ok(count)
    }
}

fn run_bundle(
    context: &Context<'_>,
    source: &BTreeMap<String, Node>,
    paths: &[String],
    report: &mut TreeReport,
) {
    let options = context.options;
    let mut bundle = options.bundle.clone();
    bundle.hashes = Some(Vec::new());
    if options.direction == Direction::Push {
        push_bundle(context, source, paths, &bundle, report);
    } else {
        pull_bundle(context, source, paths, &bundle, report);
    }
}

fn push_bundle(
    context: &Context<'_>,
    source: &BTreeMap<String, Node>,
    paths: &[String],
    bundle: &BundleOptions,
    report: &mut TreeReport,
) {
    let mut readable: Vec<String> = Vec::with_capacity(paths.len());
    let mut sources: Vec<Lazy> = Vec::with_capacity(paths.len());
    for relative in paths {
        match context.local_path(relative) {
            Ok(path) => {
                readable.push(relative.clone());
                sources.push(Lazy {
                    path,
                    file: None,
                    done: false,
                });
            }
            Err(error) => failed(report, relative, &error),
        }
    }
    let paths = readable.as_slice();
    let mut files: Vec<BundleFile<'_>> = Vec::with_capacity(paths.len());
    for (relative, lazy) in paths.iter().zip(sources.iter_mut()) {
        let mut file = BundleFile::replace(join(context.remote, relative), lazy);
        file.item.parents = true;
        file.item.modified = source.get(relative).and_then(|node| node.modified);
        file.item.file_mode = None;
        files.push(file);
    }
    match put_many(context.link, &mut files, bundle) {
        Ok(put) => {
            report.bundles += 1;
            report.wire_bytes += put.stats.wire_bytes;
            for (relative, result) in paths.iter().zip(put.results) {
                match result {
                    Ok(done) => {
                        report.bundled += 1;
                        report.bytes += done.written;
                    }
                    Err(body) => report.failures.push((
                        relative.clone(),
                        format!("{:?}: {}", body.code, body.message),
                    )),
                }
            }
        }
        Err(error) => {
            for relative in paths {
                failed(report, relative, &error);
            }
        }
    }
}

fn pull_bundle(
    context: &Context<'_>,
    source: &BTreeMap<String, Node>,
    paths: &[String],
    bundle: &BundleOptions,
    report: &mut TreeReport,
) {
    let items: Vec<ReadItem> = paths
        .iter()
        .map(|relative| ReadItem::whole(join(context.remote, relative)))
        .collect();
    let mut current: Option<Part> = None;
    let outcome = read_bundle(context.link, &items, bundle, &mut |event| {
        match event {
            Fetch::Begin(header) => {
                let at = usize::try_from(header.index.unwrap_or_default()).unwrap_or(usize::MAX);
                let relative = paths
                    .get(at)
                    .ok_or(Error::Protocol(bohio_proto::Error::Decode))?;
                current = Some(Part::create(
                    &context.local_path(relative)?,
                    relative.clone(),
                )?);
            }
            Fetch::Data(bytes) => {
                if let Some(part) = current.as_mut() {
                    part.write(bytes)?;
                }
            }
            Fetch::End { .. } => {
                if let Some(part) = current.take() {
                    let relative = part.relative.clone();
                    let node = source.get(&relative);
                    match part.finish(node) {
                        Ok(size) => {
                            report.bundled += 1;
                            report.bytes += size;
                        }
                        Err(error) => failed(report, &relative, &error),
                    }
                }
            }
            Fetch::Failed { index, error } => {
                current = None;
                let relative = usize::try_from(index)
                    .ok()
                    .and_then(|at| paths.get(at))
                    .cloned()
                    .unwrap_or_default();
                report
                    .failures
                    .push((relative, format!("{:?}: {}", error.code, error.message)));
            }
        }
        Ok(())
    });
    match outcome {
        Ok(stats) => {
            report.bundles += 1;
            report.wire_bytes += stats.wire_bytes;
        }
        Err(error) => {
            for relative in paths {
                if !report.failures.iter().any(|(path, _)| path == relative) {
                    failed(report, relative, &error);
                }
            }
        }
    }
}

/// A local file being written beside its final name, put in place when it is complete.
struct Part {
    relative: String,
    target: PathBuf,
    temp: PathBuf,
    file: Option<File>,
}

impl Part {
    fn create(target: &Path, relative: String) -> Result<Self, Error> {
        let name = target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let temp = target.with_file_name(format!(".bohio-sync-{}-{name}.tmp", std::process::id()));
        let file =
            File::create(&temp).map_err(|error| Error::local("creating a local file", &error))?;
        Ok(Self {
            relative,
            target: target.to_path_buf(),
            temp,
            file: Some(file),
        })
    }

    fn write(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.file
            .as_mut()
            .ok_or(Error::Closed)?
            .write_all(bytes)
            .map_err(|error| Error::local("writing a local file", &error))
    }

    /// Sets the source's modification time and permission bits and puts the file in place.
    fn finish(mut self, node: Option<&Node>) -> Result<u64, Error> {
        let file = self.file.take().ok_or(Error::Closed)?;
        let size = file
            .metadata()
            .map_err(|error| Error::local("looking at a local file", &error))?
            .len();
        if let Some(modified) = node.and_then(|node| node.modified).and_then(system_time) {
            file.set_modified(modified)
                .map_err(|error| Error::local("setting a local time", &error))?;
        }
        drop(file);
        #[cfg(unix)]
        if let Some(mode) = node.and_then(|node| node.mode) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.temp, fs::Permissions::from_mode(mode & 0o777));
        }
        fs::rename(&self.temp, &self.target)
            .map_err(|error| Error::local("putting a local file in place", &error))?;
        Ok(size)
    }
}

impl Drop for Part {
    fn drop(&mut self) {
        if self.file.is_some() {
            self.file = None;
            let _ = fs::remove_file(&self.temp);
        }
    }
}

fn run_single(
    context: &Context<'_>,
    source: &BTreeMap<String, Node>,
    relative: &str,
    report: &mut TreeReport,
) {
    let options = context.options;
    let mut sync = options.sync.clone();
    sync.compare = None;
    let local = match context.local_path(relative) {
        Ok(local) => local,
        Err(error) => return failed(report, relative, &error),
    };
    let remote = join(context.remote, relative);
    let node = source.get(relative);
    let moved = if options.direction == Direction::Push {
        put(context.link, &local, &remote, &sync).and_then(|moved| {
            // The next sync by size and time must find this file unchanged.
            if let Some(modified) = node.and_then(|node| node.modified) {
                let fs = Fs::connect(context.link)?.with_timeout(sync.timeout);
                let set = fs.set_times(&remote, None, Some(modified));
                fs.control().close();
                set?;
            }
            Ok(moved)
        })
    } else {
        get(context.link, &remote, &local, &sync).and_then(|moved| {
            if let Some(modified) = node.and_then(|node| node.modified).and_then(system_time) {
                File::options()
                    .write(true)
                    .open(&local)
                    .and_then(|file| file.set_modified(modified))
                    .map_err(|error| Error::local("setting a local time", &error))?;
            }
            Ok(moved)
        })
    };
    match moved {
        Ok(moved) => {
            report.singles += 1;
            report.deltas += u64::from(moved.delta);
            report.bytes += moved.literal_bytes;
            report.wire_bytes += moved.wire_bytes;
        }
        Err(error) => failed(report, relative, &error),
    }
}

#[cfg(test)]
mod tests {
    use super::refuse_name;

    #[test]
    fn a_part_that_climbs_or_is_empty_is_refused_on_every_system() {
        for windows in [false, true] {
            for bad in [
                "..",
                "a/../../x",
                "../x",
                "a/./b",
                "/abs",
                "a//b",
                "a/",
                "a\0b",
            ] {
                assert!(refuse_name(bad, windows).is_some(), "{bad:?} {windows}");
            }
            for good in ["a", "a/b/c.txt", "dir/.hidden", "a..b/c", "..a/b"] {
                assert_eq!(refuse_name(good, windows), None, "{good:?} {windows}");
            }
        }
    }

    #[test]
    fn a_name_a_windows_path_reads_as_a_drive_a_share_or_a_device_is_refused_there() {
        for bad in [
            "C:\\x",
            "C:x",
            "C:/x",
            "\\\\srv\\share\\x",
            "a\\b",
            "a/b:stream",
            "a/NUL",
            "con.txt",
            "COM1",
            "a/x.",
            "a/x ",
            "a/b?c",
        ] {
            assert!(refuse_name(bad, true).is_some(), "{bad:?}");
        }
        // The same names are ordinary on a system whose names may hold them.
        for fine in ["C:\\x", "a\\b", "a/b:stream", "a/NUL", "con.txt"] {
            assert_eq!(refuse_name(fine, false), None, "{fine:?}");
        }
    }
}
