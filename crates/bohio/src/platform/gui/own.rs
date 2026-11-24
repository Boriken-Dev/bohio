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
/// The text to report when it is not.
pub fn check_runtime_dir(dir: &Path, uid: u32) -> Result<(), String> {
    let found = std::fs::symlink_metadata(dir).map_err(|error| {
        format!(
            "the runtime directory {} cannot be read ({error}): the account's user manager is not running, enable \
             lingering for it with `loginctl enable-linger`",
            dir.display()
        )
    })?;
    if found.file_type().is_symlink() || !found.is_dir() {
        return Err(format!(
            "{} is not a directory of the account's own",
            dir.display()
        ));
    }
    if found.uid() != uid {
        return Err(format!(
            "{} belongs to uid {}, not to the node's uid {uid}",
            dir.display(),
            found.uid()
        ));
    }
    let mode = found.mode() & 0o7777;
    if mode != 0o700 {
        return Err(format!(
            "{} has mode {mode:04o}, not 0700: other accounts could reach the desktop",
            dir.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

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
        assert_eq!(check_runtime_dir(dir.path(), uid), Ok(()));
    }

    #[test]
    fn a_directory_others_can_enter_is_refused_naming_it() {
        let uid = own_uid().expect("a uid");
        for mode in [0o755, 0o710, 0o770, 0o777, 0o500] {
            let dir = temporary(mode);
            let refused = check_runtime_dir(dir.path(), uid).unwrap_err();
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
        let refused = check_runtime_dir(dir.path(), uid.wrapping_add(1)).unwrap_err();
        assert!(
            refused.contains(&dir.path().display().to_string()),
            "{refused}"
        );
        assert!(refused.contains("belongs to uid"), "{refused}");
    }

    #[test]
    fn a_link_to_a_good_directory_and_a_missing_one_are_refused() {
        let uid = own_uid().expect("a uid");
        let dir = temporary(0o700);
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(dir.path(), &link).expect("a link");
        let refused = check_runtime_dir(&link, uid).unwrap_err();
        assert!(refused.contains(&link.display().to_string()), "{refused}");
        let missing = dir.path().join("missing");
        let refused = check_runtime_dir(&missing, uid).unwrap_err();
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
