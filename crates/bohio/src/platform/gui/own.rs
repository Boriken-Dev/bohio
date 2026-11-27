//! A service account's own desktop: where its session is, from the account's uid, and whether that place is the account's.
//!
//! A node run by `systemd` as a service account serves a desktop that account's own user units made (a headless
//! compositor and an X server, `packaging/systemd/own-desktop/`). `[source.native] own_session` names which one, and the
//! node derives from its uid what the command would have read from a login session's environment: the runtime directory
//! (`/run/user/UID`) and the display (`:UID`). The runtime directory is checked first: a directory that is not the
//! account's, or that others can enter, is not a place to look for a screen.

#![forbid(unsafe_code)]

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Which kind of desktop the account's units run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OwnSession {
    /// A Wayland compositor and the desktop portals: a view of every output, input where the portal has it.
    Wayland,
    /// An X server on the compositor: capture and XTest input of its screen.
    X11,
}

impl OwnSession {
    /// `wayland` or `x11`; `none` and anything else is `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "wayland" => Some(Self::Wayland),
            "x11" => Some(Self::X11),
            _ => None,
        }
    }
}

/// Why a runtime directory is not the account's own.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RuntimeDirError {
    /// The directory cannot be read: the account's user manager is not running.
    #[error(
        "the runtime directory {} cannot be read ({reason}): the account's user manager is not running, enable lingering for it with `loginctl enable-linger`",
        dir.display()
    )]
    Unreadable {
        /// The directory.
        dir: PathBuf,
        /// The operating system's error.
        reason: String,
    },
    /// The path is a link, or not a directory.
    #[error("{} is not a directory of the account's own", dir.display())]
    NotADirectory {
        /// The directory.
        dir: PathBuf,
    },
    /// Another account owns it.
    #[error("{} belongs to uid {owner}, not to the node's uid {uid}", dir.display())]
    WrongOwner {
        /// The directory.
        dir: PathBuf,
        /// Its owner's uid.
        owner: u32,
        /// The uid it should belong to.
        uid: u32,
    },
    /// Others can enter it.
    #[error("{} has mode {mode:04o}, not 0700: other accounts could reach the desktop", dir.display())]
    WrongMode {
        /// The directory.
        dir: PathBuf,
        /// Its permission bits.
        mode: u32,
    },
}

/// The user id this process runs as, as the kernel reports it for the process itself.
///
/// # Errors
///
/// `/proc` is not readable.
pub fn own_uid() -> std::io::Result<u32> {
    Ok(std::fs::metadata("/proc/self")?.uid())
}

/// The runtime directory of the account with `uid`: `/run/user/UID`.
#[must_use]
pub fn runtime_dir(uid: u32) -> PathBuf {
    PathBuf::from(format!("/run/user/{uid}"))
}

/// The display the account's X server listens on: `:UID`, so that accounts on one host never share a name.
#[must_use]
pub fn display(uid: u32) -> String {
    format!(":{uid}")
}

/// Whether `dir` is a directory (not a link to one) that `uid` owns and only `uid` can enter, naming `dir` in the
/// refusal.
///
/// # Errors
///
/// [`RuntimeDirError`] says why it is not. With `check_owner` false (`own_runtime_dir_check = false`) only the link and
/// directory test is made.
pub fn check_runtime_dir(dir: &Path, uid: u32, check_owner: bool) -> Result<(), RuntimeDirError> {
    let found = std::fs::symlink_metadata(dir).map_err(|error| RuntimeDirError::Unreadable {
        dir: dir.to_owned(),
        reason: error.to_string(),
    })?;
    if found.file_type().is_symlink() || !found.is_dir() {
        return Err(RuntimeDirError::NotADirectory {
            dir: dir.to_owned(),
        });
    }
    if !check_owner {
        return Ok(());
    }
    if found.uid() != uid {
        return Err(RuntimeDirError::WrongOwner {
            dir: dir.to_owned(),
            owner: found.uid(),
            uid,
        });
    }
    let mode = found.mode() & 0o7777;
    if mode != 0o700 {
        return Err(RuntimeDirError::WrongMode {
            dir: dir.to_owned(),
            mode,
        });
    }
    Ok(())
}

/// The connection to the own desktop's X server: its abstract socket, and a check that the account running it is the node's.
///
/// The abstract name is host-wide, so any account can bind it first; the node would then send typed text to it and take its
/// pictures. The kernel's answer about the peer of the socket (`SO_PEERCRED`) is the account that listens, whatever it says.
#[derive(Debug)]
pub struct OwnServer {
    /// The abstract socket's name, without the leading NUL: `/tmp/.X11-unix/X<uid>`.
    name: String,
    /// The account that must run the server.
    uid: u32,
}

impl OwnServer {
    /// The server of the display `:uid`, which only the account `uid` may run.
    #[must_use]
    pub fn new(uid: u32) -> Self {
        Self {
            name: format!("/tmp/.X11-unix/X{uid}"),
            uid,
        }
    }

    /// As [`OwnServer::new`], for a socket of another name (a test's).
    #[doc(hidden)]
    #[must_use]
    pub fn named(name: &str, uid: u32) -> Self {
        Self {
            name: name.to_owned(),
            uid,
        }
    }
}

impl bohio_x11::Connector for OwnServer {
    fn connect(&self) -> std::io::Result<bohio_x11::Wire> {
        use std::os::linux::net::SocketAddrExt as _;
        use std::os::unix::net::{SocketAddr, UnixStream};
        let address = SocketAddr::from_abstract_name(&self.name)?;
        let stream = UnixStream::connect_addr(&address)?;
        let found = rustix::net::sockopt::socket_peercred(&stream)?;
        if found.uid.as_raw() != self.uid {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "the X server on @{} is run by uid {}, not by the node's uid {}: another account holds the display's name \
                     (`own_session_check_server = false` serves it anyway)",
                    self.name,
                    found.uid.as_raw(),
                    self.uid
                ),
            ));
        }
        Ok(bohio_x11::Wire::new(stream.into(), Vec::new(), Vec::new()))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use bohio_x11::Connector as _;

    use super::*;

    /// Listens on an abstract name only this test uses.
    fn listening(tag: &str) -> (String, std::os::unix::net::UnixListener) {
        use std::os::linux::net::SocketAddrExt as _;
        let name = format!("/tmp/.X11-unix/bohio-test-{}-{tag}", std::process::id());
        let address = std::os::unix::net::SocketAddr::from_abstract_name(&name).expect("a name");
        let listener = std::os::unix::net::UnixListener::bind_addr(&address).expect("listens");
        (name, listener)
    }

    #[test]
    fn a_server_run_by_the_nodes_account_is_connected() {
        let (name, listener) = listening("own");
        let uid = own_uid().expect("a uid");
        OwnServer::named(&name, uid)
            .connect()
            .expect("the node's own account serves it");
        drop(listener);
    }

    #[test]
    fn a_server_run_by_another_account_is_refused_naming_both() {
        let (name, listener) = listening("other");
        let uid = own_uid().expect("a uid");
        // The listener is this account's: a node of another uid sees a stranger.
        let error = OwnServer::named(&name, uid.wrapping_add(1))
            .connect()
            .expect_err("a stranger holds the name");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        let text = error.to_string();
        assert!(text.contains(&format!("run by uid {uid}")), "{text}");
        assert!(
            text.contains(&format!("node's uid {}", uid.wrapping_add(1))),
            "{text}"
        );
        drop(listener);
    }

    #[test]
    fn nothing_listening_is_an_error_not_a_stranger() {
        let error = OwnServer::new(own_uid().expect("a uid").wrapping_add(4_000_000))
            .connect()
            .expect_err("no server");
        assert_ne!(error.kind(), std::io::ErrorKind::PermissionDenied);
    }

    fn temporary(mode: u32) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("a directory");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(mode))
            .expect("its mode");
        dir
    }

    #[test]
    fn a_directory_of_the_account_with_mode_0700_is_the_account_s() {
        let uid = own_uid().expect("a uid");
        let dir = temporary(0o700);
        assert_eq!(check_runtime_dir(dir.path(), uid, true), Ok(()));
    }

    #[test]
    fn a_directory_others_can_enter_is_refused_naming_it() {
        let uid = own_uid().expect("a uid");
        for mode in [0o755, 0o710, 0o770, 0o777, 0o500] {
            let dir = temporary(mode);
            let refused = check_runtime_dir(dir.path(), uid, true)
                .unwrap_err()
                .to_string();
            assert!(
                refused.contains(&dir.path().display().to_string()),
                "{refused}"
            );
            assert!(refused.contains(&format!("{mode:04o}")), "{refused}");
        }
    }

    #[test]
    fn a_directory_of_another_uid_is_refused_naming_it() {
        let uid = own_uid().expect("a uid");
        let dir = temporary(0o700);
        let refused = check_runtime_dir(dir.path(), uid.wrapping_add(1), true)
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains(&dir.path().display().to_string()),
            "{refused}"
        );
        assert!(refused.contains("belongs to uid"), "{refused}");
    }

    /// `own_runtime_dir_check = false` drops the owner and mode tests and keeps the link and directory test.
    #[test]
    fn the_owner_and_mode_tests_can_be_dropped_and_the_link_test_cannot() {
        let uid = own_uid().expect("a uid");
        let open = temporary(0o755);
        assert_eq!(check_runtime_dir(open.path(), uid, false), Ok(()));
        assert_eq!(
            check_runtime_dir(open.path(), uid.wrapping_add(1), false),
            Ok(())
        );
        let link = open.path().join("link");
        std::os::unix::fs::symlink(open.path(), &link).expect("a link");
        assert!(check_runtime_dir(&link, uid, false).is_err());
    }

    #[test]
    fn a_link_to_a_good_directory_and_a_missing_one_are_refused() {
        let uid = own_uid().expect("a uid");
        let dir = temporary(0o700);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path(), &link).expect("a link");
        let refused = check_runtime_dir(&link, uid, true).unwrap_err().to_string();
        assert!(refused.contains(&link.display().to_string()), "{refused}");
        let missing = dir.path().join("missing");
        let refused = check_runtime_dir(&missing, uid, true)
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains(&missing.display().to_string()),
            "{refused}"
        );
        assert!(refused.contains("enable-linger"), "{refused}");
    }

    #[test]
    fn the_names_come_from_the_uid() {
        assert_eq!(runtime_dir(1001), PathBuf::from("/run/user/1001"));
        assert_eq!(display(1001), ":1001");
        assert_eq!(OwnSession::parse("x11"), Some(OwnSession::X11));
        assert_eq!(OwnSession::parse("wayland"), Some(OwnSession::Wayland));
        assert_eq!(OwnSession::parse("none"), None);
    }
}
