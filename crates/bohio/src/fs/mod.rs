//! Filesystem access confined to a peer's roots: the control calls and the two transfer streams.
//!
//! [`call`] answers one filesystem call of the control stream; [`serve_read`] and [`serve_write`]
//! serve a `bohio-read/1` and a `bohio-write/1` stream. Every path is resolved against the
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
//!   (`NtCreateFile` with a `RootDirectory`, reparse points not followed), so `stat`, `checksum`, reads
//!   and the opens of writes are race-free. Listing a directory, reading a link, creating a
//!   directory, removing, renaming, symlinking and changing permissions are not: the library
//!   turns the parent's handle into its current path and acts on the parent's path joined with the name, so a peer or local
//!   user who can rename a directory inside the root during that call can redirect it to wherever
//!   that name then points. The root itself is opened by its path once per call, so what lies
//!   above a configured root is the administrator's to protect.
//!
//! On every system the host still trusts the roots' owner: a hard link made by another local user
//! to a file outside a root exposes that file, and a bind mount or a Windows junction an
//! administrator places inside a root is part of the root.

mod ops;
mod path;
mod read;
mod stage;
mod write;

use bohio_proto::control::{Call, Reply};
use bohio_proto::{ErrorBody, ErrorCode};

use crate::config::FilesystemConfig;
use crate::limit::Slots;
use crate::policy::{Grant, Permission};

pub use read::serve_read;
pub use write::serve_write;

use ops::Roots;

/// What the filesystem engines need besides the stream.
#[derive(Debug, Clone, Copy)]
pub struct Context<'a> {
    /// What this peer may do, and the roots its file operations are confined to.
    pub grant: &'a Grant,
    /// The `[filesystem]` settings.
    pub config: &'a FilesystemConfig,
    /// The bound on concurrent transfers, shared by every stream.
    pub slots: &'a Slots,
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
        Call::Stat { .. } | Call::List { .. } | Call::Readlink { .. } | Call::Checksum { .. } => {
            &[FsRead]
        }
        Call::Copy { .. } => &[FsRead, FsWrite],
        Call::Mkdir { .. }
        | Call::Remove { .. }
        | Call::Rename { .. }
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
    let roots = Roots::new(context.grant.fs_roots());
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
        Call::Copy {
            from,
            to,
            overwrite,
        } => {
            ops::copy(&roots, context, from, to, *overwrite)?;
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
        Call::Checksum {
            path,
            offset,
            length,
        } => {
            let (sha256, length) = ops::checksum(&roots, context, path, *offset, *length)?;
            Reply::Checksum { sha256, length }
        }
        _ => unreachable!("every other call returned above"),
    })
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
