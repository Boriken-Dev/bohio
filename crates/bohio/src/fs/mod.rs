//! Filesystem access confined to a peer's roots: the control calls and the two transfer streams.
//!
//! [`call`] answers one filesystem call of the control stream; [`serve_read`], [`serve_write`] and
//! [`serve_hash`] serve a `bohio-read/1`, a `bohio-write/1` and a `bohio-hash/1` stream. Every path is resolved against the
//! directory handle of the granted root it lies under, so a `..` or a symbolic link on the way
//! cannot lead out of it.
//!
//! # Confinement
//!
//! A path is first resolved lexically: `.` and `..` are folded away, a `..` that would leave the
//! path's start is `outside_root`, and an absolute path must lie under a root (compared without
//! case on Windows and macOS). What remains is opened relative to the root's directory handle by
//! `cap-std`, which follows a symbolic link only while it stays under that handle and otherwise
//! refuses (`outside_root`): an absolute link is refused even when it points back inside.
//!
//! What is race-free depends on the operating system:
//!
//! - Linux: every lookup is one `openat2` with `RESOLVE_BENEATH` (kernel 5.6 and later); an older
//!   kernel resolves component by component with `openat` and `O_NOFOLLOW`, each relative to the
//!   handle of the component before. A link swapped in between components is seen by the next
//!   `openat`, so there is no check-then-use gap. Changes and removals are `*at` calls on the
//!   parent's handle.
//! - macOS and other Unix: the component-by-component `openat` walk with `O_NOFOLLOW`; the same
//!   guarantee, and `*at` calls on the parent's handle.
//! - Windows: walking a path and opening a file or directory are relative to the root's handle
//!   (`NtCreateFile` with a `RootDirectory`, reparse points not followed), and so are listing a
//!   directory, reading and making a link, creating a directory, removing (a tree too), renaming and
//!   setting the read-only attribute (`platform::winfs`): the name is opened relative to the handle of
//!   the directory that holds it and the call goes through the handle it gets, so no path is built
//!   and a directory swapped for a junction cannot redirect it. The root itself is opened by its path
//!   once per call, so what lies above a configured root is the administrator's to protect.
//!
//! # Through a files source
//!
//! The engines never name `cap-std`: a root is opened as a directory handle by the node's [`bohio_api::FilesSource`]
//! (`native` by default), and `vfs` gives the engines its handles, files and attributes. What each guarantee above
//! means is the source's: `native` keeps all of it inside its provider, and a source that resolves names by path says
//! so (`describe` lacks `confined`). Within `native` one step is weaker than before: the attributes of a listed entry are
//! asked by its name below the directory the listing was opened on, where they were read through the listing's own handle,
//! so a directory on that path swapped for a link between the listing and the question can answer for another directory,
//! still inside the root. A rename between two roots opens both roots by their paths in the provider, as each call did.
//!
//! On every system the host still trusts the roots' owner: a hard link made by another local user
//! to a file outside a root exposes that file, and a bind mount or a Windows junction an
//! administrator places inside a root is part of the root.

#![forbid(unsafe_code)]

mod bundle;
pub(crate) mod compress;
mod delta;
mod follow;
mod hash;
pub(crate) mod native;
mod ops;
mod path;
mod prepare;
mod read;
mod search;
mod stage;
mod tree;
mod vfs;
mod walk;
mod write;

use bohio_proto::control::{Call, Reply};
use bohio_proto::{ErrorBody, ErrorCode};

use crate::config::{CompressionConfig, DeltaConfig, FilesystemConfig, HashConfig};
use crate::control::Current;
use crate::limit::Slots;
use crate::policy::{Grant, Permission};

/// A reader of an image of the node: a regular file opened inside the roots, and its size.
pub(crate) struct Confined {
    file: vfs::File,
    size: u64,
}

impl Confined {
    /// The file's size when it was opened.
    pub(crate) const fn size(&self) -> u64 {
        self.size
    }

    /// Reads at `offset` into `dst`, and answers the bytes read.
    pub(crate) fn read_at(&self, offset: u64, dst: &mut [u8]) -> std::io::Result<usize> {
        self.file.read_at(offset, dst)
    }
}

/// Opens the regular file `path` for reading inside `roots`, as a read of it would, and says its size: what a source that
/// reads an image of the node is given.
///
/// # Errors
///
/// `outside_root`, `not_found`, `denied` and the other refusals of a read's open.
pub(crate) fn open_confined(
    roots: &[std::path::PathBuf],
    files: &std::sync::Arc<dyn bohio_api::FilesSource>,
    path: &str,
) -> Result<Confined, bohio_proto::ErrorBody> {
    let roots = ops::Roots::new(roots, files);
    let (file, meta) = ops::open_regular(&roots, path)?;
    Ok(Confined {
        size: meta.len(),
        file,
    })
}

/// The files source an engine reaches the file system through.
#[derive(Clone, Copy)]
pub struct FilesRef<'a>(pub &'a std::sync::Arc<dyn bohio_api::FilesSource>);

impl core::fmt::Debug for FilesRef<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FilesRef").finish_non_exhaustive()
    }
}

pub use hash::{HashContext, serve_hash};
pub use prepare::sweep as sweep_prepared;
pub use read::serve_read;
pub use search::{SearchContext, serve_search};
pub use stage::sweep_roots as sweep_resumable;
pub use walk::{WalkContext, serve_walk};
pub use write::serve_write;

use ops::Roots;

/// What the filesystem engines need besides the stream.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// What this peer may do, and the roots its file operations are confined to.
    pub grant: &'a Grant,
    /// The `[filesystem]` settings.
    pub config: &'a FilesystemConfig,
    /// The `[hash]` settings: which digests reads and writes may name, and the hash streams' bounds.
    pub hash: &'a HashConfig,
    /// The `[delta]` settings: the block bounds of signatures and the bounds of delta reads and writes.
    pub delta: &'a DeltaConfig,
    /// The `[compression]` settings: what reads and writes may compress and how they are bounded.
    pub compression: &'a CompressionConfig,
    /// The peer's id as text: a prepared read is bound to the peer that made it.
    pub peer: &'a str,
    /// The files source the roots are opened from.
    pub source: FilesRef<'a>,
    /// The bound on concurrent transfers, shared by every stream.
    pub slots: &'a Slots,
    /// The bound on concurrent followed reads, shared by every stream.
    pub follows: &'a Slots,
    /// Where the peer's grant is read before each item of a bundle, so a grant narrowed while the stream is open
    /// applies to the next item; `grant` is used when absent.
    pub current: Option<Current<'a>>,
}

/// Answers one filesystem call of the control stream.
///
/// `budget` is the largest reply the stream carries; a listing page is cut to fit it. Checks, in
/// order: the filesystem is enabled (`unsupported`), the grant holds the permission the call needs
/// (`denied`), the grant names at least one root (`denied`), the path (`bad_request`,
/// `outside_root`), then the operation (`not_found`, `exists`, `denied`, `io`, ...).
///
/// # Errors
///
/// The [`ErrorBody`] to send back. A call that is not a filesystem call is `bad_request`.
pub fn call(context: &Context<'_>, call: &Call, budget: usize) -> Result<Reply, ErrorBody> {
    use Permission::{FsRead, FsWrite};
    let needs: &[Permission] = match call {
        Call::Stat { .. } | Call::List { .. } | Call::Readlink { .. } => &[FsRead],
        Call::Copy { .. } => &[FsRead, FsWrite],
        Call::Mkdir { .. }
        | Call::Remove { .. }
        | Call::Rename { .. }
        | Call::Move { .. }
        | Call::Truncate { .. }
        | Call::Symlink { .. }
        | Call::SetTimes { .. }
        | Call::SetMode { .. }
        | Call::SetAttributes { .. } => &[FsWrite],
        _ => {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "that is not a filesystem call",
            ));
        }
    };
    authorize(context, needs)?;
    let roots = Roots::new(context.grant.fs_roots(), context.source.0);
    Ok(match call {
        Call::Stat { path, follow } => Reply::Stat(Box::new(ops::stat(&roots, path, *follow)?)),
        Call::List { path, after, limit } => Reply::List(ops::list(
            &roots,
            context,
            path,
            after.as_deref(),
            *limit,
            budget,
        )?),
        Call::Mkdir {
            path,
            parents,
            mode,
        } => {
            ops::mkdir(&roots, path, *parents, *mode)?;
            Reply::Done
        }
        Call::Remove { path, recursive } => {
            ops::remove(&roots, path, *recursive)?;
            Reply::Done
        }
        Call::Rename {
            from,
            to,
            overwrite,
        } => {
            ops::rename(&roots, from, to, *overwrite)?;
            Reply::Done
        }
        Call::Move {
            from,
            to,
            overwrite,
        } => {
            tree::move_to(&roots, context, from, to, *overwrite)?;
            Reply::Done
        }
        Call::Truncate { path, length } => {
            tree::truncate(&roots, path, *length)?;
            Reply::Done
        }
        Call::Copy {
            from,
            to,
            overwrite,
            recursive,
            preserve,
        } => {
            let options = tree::CopyOptions {
                overwrite: *overwrite,
                recursive: *recursive,
                preserve: *preserve,
            };
            tree::copy(&roots, context, from, to, &options)?;
            Reply::Done
        }
        Call::Symlink { target, link } => {
            ops::symlink(&roots, target, link)?;
            Reply::Done
        }
        Call::Readlink { path } => Reply::Link {
            target: ops::readlink(&roots, path)?,
        },
        Call::SetTimes {
            path,
            accessed,
            modified,
        } => {
            ops::set_times(&roots, path, *accessed, *modified)?;
            Reply::Done
        }
        Call::SetMode { path, mode } => {
            ops::set_mode(&roots, path, *mode)?;
            Reply::Done
        }
        Call::SetAttributes { path, readonly } => {
            ops::set_attributes(&roots, path, *readonly)?;
            Reply::Done
        }
        _ => unreachable!("every other call returned above"),
    })
}

/// Whether the file a `bohio-read/1` stream opened with still lies under one of `grant`'s roots. A
/// request that cannot be read is covered, so [`serve_read`] answers it.
pub(crate) fn covers_read(grant: &Grant, metadata: &[u8]) -> bool {
    bohio_proto::fs::ReadRequest::from_metadata(metadata)
        .map_or(true, |request| under_a_root(grant, &request.path))
}

/// As [`covers_read`], for a `bohio-write/1` stream. A delta write also needs the peer to hold `fs.read`: it
/// reads the target.
pub(crate) fn covers_write(grant: &Grant, metadata: &[u8]) -> bool {
    bohio_proto::fs::WriteRequest::from_metadata(metadata).map_or(true, |request| {
        under_a_root(grant, &request.path)
            && ((!matches!(request.mode, bohio_proto::fs::WriteMode::Delta { .. })
                && request.base.is_none())
                || grant.allows(Permission::FsRead))
    })
}

/// Whether `text` lies under a root of `grant` by its words: a path that does not parse is the
/// engine's to refuse, and one that leaves every root, or a grant with no root, is not covered.
fn under_a_root(grant: &Grant, text: &str) -> bool {
    !grant.fs_roots().is_empty()
        && path::locate(grant.fs_roots(), text)
            .map_or_else(|error| error.code != ErrorCode::OutsideRoot, |_| true)
}

/// The set-user-ID and set-group-ID bits: a file carrying one runs as its owner or group, the service's
/// account, for whoever starts it, so no peer sets them and no copy or replacement keeps them.
pub(crate) const ID_BITS: u32 = 0o6000;

/// Checks a mode a peer asks for: at most `0o7777` (`bad_request`) and without [`ID_BITS`] (`denied`).
pub(crate) fn check_mode(mode: u32) -> Result<(), ErrorBody> {
    if mode > 0o7777 {
        return Err(ErrorBody::new(
            ErrorCode::BadRequest,
            "a mode is at most 0o7777",
        ));
    }
    if mode & ID_BITS != 0 {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            "a peer may not set the set-user-ID or set-group-ID bit",
        ));
    }
    Ok(())
}

/// The checks every filesystem operation starts with.
pub(crate) fn authorize(context: &Context<'_>, needs: &[Permission]) -> Result<(), ErrorBody> {
    if !context.config.enabled {
        return Err(ErrorBody::new(
            ErrorCode::Unsupported,
            "this host does not serve files",
        ));
    }
    for permission in needs {
        if !context.grant.allows(*permission) {
            return Err(ErrorBody::new(
                ErrorCode::Denied,
                format!("this peer does not hold {}", permission.name()),
            ));
        }
    }
    if context.grant.fs_roots().is_empty() {
        return Err(ErrorBody::new(
            ErrorCode::Denied,
            "this peer has no filesystem root",
        ));
    }
    Ok(())
}

/// The structured error for an operating system failure. It names what happened, never a path.
pub(crate) fn io_error(error: &std::io::Error) -> ErrorBody {
    use std::io::ErrorKind as Kind;
    let (code, message) = match error.kind() {
        Kind::PermissionDenied if error.to_string().contains("outside of the filesystem") => {
            return path::outside_root();
        }
        Kind::PermissionDenied => (ErrorCode::Denied, "the operating system refused access"),
        Kind::NotFound => (ErrorCode::NotFound, "the path does not exist"),
        Kind::AlreadyExists => (ErrorCode::Exists, "the target exists"),
        Kind::NotADirectory => (
            ErrorCode::BadRequest,
            "a name on the path is not a directory",
        ),
        Kind::IsADirectory => (ErrorCode::BadRequest, "the path is a directory"),
        Kind::DirectoryNotEmpty => (ErrorCode::BadRequest, "the directory is not empty"),
        Kind::InvalidInput | Kind::InvalidFilename => (
            ErrorCode::BadRequest,
            "the operating system rejected the path",
        ),
        Kind::Unsupported => (
            ErrorCode::Unsupported,
            "the operating system does not offer this",
        ),
        _ => (ErrorCode::Io, "the operation failed"),
    };
    ErrorBody::new(code, message)
}

/// The source of a node that serves no files: every root is unavailable.
#[must_use]
pub fn absent() -> std::sync::Arc<dyn bohio_api::FilesSource> {
    struct Absent;

    impl bohio_api::FilesSource for Absent {
        fn describe(
            &self,
        ) -> Result<bohio_api::SourceInfo, bohio_api::guatiao::library::ProviderError> {
            Ok(bohio_api::SourceInfo::new("none", &[], &[])
                .unavailable("this node serves no files"))
        }

        fn open_dir(
            &self,
            _path: &str,
        ) -> Result<
            bohio_api::guatiao::library::Object<dyn bohio_api::Directory>,
            bohio_api::guatiao::library::ProviderError,
        > {
            Err(bohio_api::unavailable("this node serves no files"))
        }
    }

    std::sync::Arc::new(Absent)
}
