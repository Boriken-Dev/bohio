//! Mapping a peer's path text onto one of its granted roots.
//!
//! This is the lexical half of confinement: it decides which root a path is under and what is left
//! of it, with `.` and `..` resolved and every component checked. It never touches the disk. The
//! other half, that no symbolic link on the way leads out, belongs to the directory handle the
//! rest is resolved against.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf, Prefix};

use bohio_proto::{ErrorBody, ErrorCode};

/// Whether this host's usual filesystems treat names that differ only in case as one name.
const FOLD: bool = cfg!(any(windows, target_os = "macos"));

/// A path found to lie under a root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Located {
    /// The index of the root in the grant.
    pub root: usize,
    /// What follows the root: plain names only, empty for the root itself.
    pub relative: PathBuf,
}

/// The error for a path that leaves the granted roots.
pub(crate) fn outside_root() -> ErrorBody {
    ErrorBody::new(
        ErrorCode::OutsideRoot,
        "the path leaves the directories this peer may use",
    )
}

fn bad(message: &'static str) -> ErrorBody {
    ErrorBody::new(ErrorCode::BadRequest, message)
}

struct Parsed {
    /// The drive or share, folded; Windows only.
    prefix: Option<String>,
    rooted: bool,
    names: Vec<OsString>,
}

impl Parsed {
    fn is_absolute(&self) -> bool {
        self.prefix.is_some() || self.rooted
    }
}

fn fold(name: &OsStr) -> String {
    let text = name.to_string_lossy();
    if FOLD {
        text.to_lowercase()
    } else {
        text.into_owned()
    }
}

fn same(a: &OsStr, b: &OsStr) -> bool {
    if FOLD { fold(a) == fold(b) } else { a == b }
}

fn prefix_key(kind: Prefix<'_>) -> Result<String, ErrorBody> {
    match kind {
        Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
            Ok(format!("{}:", char::from(letter).to_ascii_lowercase()))
        }
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
            Ok(format!("\\\\{}\\{}", fold(server), fold(share)))
        }
        Prefix::Verbatim(_) | Prefix::DeviceNS(_) => Err(bad("this form of path is not accepted")),
    }
}

/// Windows names the OS resolves to a device or a data stream instead of a file.
fn check_name(name: &OsStr) -> Result<(), ErrorBody> {
    if !cfg!(windows) {
        return Ok(());
    }
    let text = name.to_string_lossy();
    if text.contains(':') {
        return Err(bad("a name may not hold a colon"));
    }
    if text.ends_with(' ') || text.ends_with('.') {
        return Err(bad("a name may not end in a space or a dot"));
    }
    let stem = text
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_ascii_uppercase();
    let device = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|digit| {
                matches!(digit, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9")
            });
    if device {
        return Err(bad("a name may not be a device name"));
    }
    Ok(())
}

fn parse(text: &str, names_checked: bool) -> Result<Parsed, ErrorBody> {
    let native;
    let text = if cfg!(windows) && text.contains('/') {
        native = text.replace('/', "\\");
        native.as_str()
    } else {
        text
    };
    let mut parsed = Parsed {
        prefix: None,
        rooted: false,
        names: Vec::new(),
    };
    for component in Path::new(text).components() {
        match component {
            Component::Prefix(prefix) => parsed.prefix = Some(prefix_key(prefix.kind())?),
            Component::RootDir => parsed.rooted = true,
            Component::CurDir => {}
            Component::ParentDir => {
                if parsed.names.pop().is_none() {
                    return Err(outside_root());
                }
            }
            Component::Normal(name) => {
                if names_checked {
                    check_name(name)?;
                }
                parsed.names.push(name.to_owned());
            }
        }
    }
    Ok(parsed)
}

/// Finds the root `text` lies under and what is left of it.
///
/// An absolute path must lie under a root; the longest matching root wins. A relative path is taken
/// from the first root. `..` is resolved lexically and is refused when it would leave the path's
/// own start. Roots compare case-insensitively on Windows and macOS.
///
/// # Errors
///
/// `bad_request` for an empty path, a NUL, a Windows device, stream or drive-relative form;
/// `outside_root` for a path that leaves every root.
#[cfg(test)]
pub(crate) fn locate(roots: &[PathBuf], text: &str) -> Result<Located, ErrorBody> {
    locate_in(roots, text, Syntax::Host)
}

/// How the names of a source are written: as this system writes them, or as POSIX does (`/` separates, one root, names
/// compare as they are). A Windows node serving a Linux machine names its files in the second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Syntax {
    /// This system's own paths.
    Host,
    /// POSIX paths.
    Posix,
}

/// The parts of a POSIX path: rooted or not, and its names, with `.` and empty parts dropped and `..` folded.
fn parse_posix(text: &str) -> Result<Parsed, ErrorBody> {
    let mut parsed = Parsed {
        prefix: None,
        rooted: text.starts_with('/'),
        names: Vec::new(),
    };
    for part in text.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parsed.names.pop().is_none() {
                    return Err(outside_root());
                }
            }
            // On a Windows node a name is turned into a path of the node's own kind, which splits at a backslash: a name
            // that holds one cannot be carried as one name.
            name if cfg!(windows) && name.contains('\\') => {
                return Err(bad(
                    "a POSIX name cannot hold a backslash on a Windows node",
                ));
            }
            name => parsed.names.push(OsString::from(name)),
        }
    }
    Ok(parsed)
}

/// As [`locate`], for names written in `syntax`.
///
/// # Errors
///
/// As [`locate`].
pub(crate) fn locate_in(
    roots: &[PathBuf],
    text: &str,
    syntax: Syntax,
) -> Result<Located, ErrorBody> {
    let posix = syntax == Syntax::Posix;
    let parse = |text: &str, checked: bool| {
        if posix {
            parse_posix(text)
        } else {
            parse(text, checked)
        }
    };
    let same = |a: &OsStr, b: &OsStr| if posix { a == b } else { same(a, b) };
    if text.is_empty() {
        return Err(bad("the path is empty"));
    }
    if text.contains('\0') {
        return Err(bad("the path holds a NUL"));
    }
    let parsed = parse(text, true)?;
    if !parsed.is_absolute() {
        return Ok(Located {
            root: 0,
            relative: parsed.names.into_iter().collect(),
        });
    }
    if parsed.prefix.is_some() && !parsed.rooted {
        return Err(bad("a drive-relative path is not accepted"));
    }
    let mut best: Option<(usize, usize)> = None;
    for (index, root) in roots.iter().enumerate() {
        let Ok(root) = parse(&root.to_string_lossy(), false) else {
            continue;
        };
        let under = root.is_absolute()
            && root.prefix == parsed.prefix
            && root.rooted == parsed.rooted
            && root.names.len() <= parsed.names.len()
            && root
                .names
                .iter()
                .zip(&parsed.names)
                .all(|(a, b)| same(a, b));
        if under && best.is_none_or(|(_, longest)| root.names.len() > longest) {
            best = Some((index, root.names.len()));
        }
    }
    let (root, skip) = best.ok_or_else(outside_root)?;
    Ok(Located {
        root,
        relative: parsed.names[skip..].iter().collect(),
    })
}

/// Checks that a symbolic link's text names something below the link's own directory: not
/// absolute, and never more `..` than the directories above it inside the root.
///
/// `depth` is how many names lie between the root and the directory that holds the link.
pub(crate) fn check_link_target(target: &str, depth: usize, posix: bool) -> Result<(), ErrorBody> {
    if target.is_empty() {
        return Err(bad("the link target is empty"));
    }
    if target.contains('\0') {
        return Err(bad("the link target holds a NUL"));
    }
    if posix {
        // A source that names its files as POSIX does is told the text as it is written, and judged by POSIX rules,
        // whatever this node's own syntax is.
        if target.starts_with('/') {
            return Err(outside_root());
        }
        let mut depth = i64::try_from(depth).unwrap_or(i64::MAX);
        for part in target.split('/') {
            match part {
                "" | "." => {}
                ".." => {
                    depth -= 1;
                    if depth < 0 {
                        return Err(outside_root());
                    }
                }
                _ => depth = depth.saturating_add(1),
            }
        }
        return Ok(());
    }
    let text = if cfg!(windows) {
        target.replace('/', "\\")
    } else {
        target.to_owned()
    };
    let mut depth = i64::try_from(depth).unwrap_or(i64::MAX);
    for component in Path::new(&text).components() {
        match component {
            Component::Prefix(_) | Component::RootDir => return Err(outside_root()),
            Component::CurDir => {}
            Component::ParentDir => {
                depth -= 1;
                if depth < 0 {
                    return Err(outside_root());
                }
            }
            Component::Normal(name) => {
                check_name(name)?;
                depth = depth.saturating_add(1);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roots(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    fn code(result: Result<Located, ErrorBody>) -> ErrorCode {
        result.expect_err("refused").code
    }

    #[cfg(unix)]
    #[test]
    fn an_absolute_path_under_a_root_is_split_from_it() {
        let found = locate(&roots(&["/srv/data"]), "/srv/data/a//b/./c").unwrap();
        assert_eq!(found.root, 0);
        assert_eq!(found.relative, PathBuf::from("a/b/c"));
        assert_eq!(
            locate(&roots(&["/srv/data"]), "/srv/data")
                .unwrap()
                .relative,
            PathBuf::new()
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_longest_matching_root_wins_and_a_sibling_prefix_does_not_match() {
        let list = roots(&["/srv", "/srv/data"]);
        assert_eq!(locate(&list, "/srv/data/x").unwrap().root, 1);
        assert_eq!(
            code(locate(&roots(&["/srv/data"]), "/srv/database/x")),
            ErrorCode::OutsideRoot
        );
    }

    #[cfg(unix)]
    #[test]
    fn dotdot_that_leaves_the_path_or_every_root_is_outside_root() {
        let list = roots(&["/srv/data"]);
        assert_eq!(code(locate(&list, "../x")), ErrorCode::OutsideRoot);
        assert_eq!(code(locate(&list, "a/../../x")), ErrorCode::OutsideRoot);
        assert_eq!(
            code(locate(&list, "/srv/data/../etc/passwd")),
            ErrorCode::OutsideRoot
        );
        assert_eq!(code(locate(&list, "/etc/passwd")), ErrorCode::OutsideRoot);
        assert_eq!(
            locate(&list, "a/../b").unwrap().relative,
            PathBuf::from("b")
        );
    }

    #[test]
    fn a_relative_path_is_taken_from_the_first_root() {
        let found = locate(&roots(&["/first", "/second"]), "x/y").unwrap();
        assert_eq!(found.root, 0);
        assert_eq!(found.relative, PathBuf::from("x/y"));
    }

    #[test]
    fn an_empty_path_and_a_nul_are_bad_requests() {
        assert_eq!(code(locate(&roots(&["/r"]), "")), ErrorCode::BadRequest);
        assert_eq!(code(locate(&roots(&["/r"]), "a\0b")), ErrorCode::BadRequest);
    }

    #[test]
    fn a_link_target_may_not_be_absolute_or_climb_out() {
        // A POSIX source is judged by POSIX rules whatever the node's own are.
        assert!(check_link_target("a/b", 0, true).is_ok());
        assert!(check_link_target("../up", 1, true).is_ok());
        assert_eq!(
            check_link_target("../up", 0, true).unwrap_err().code,
            ErrorCode::OutsideRoot
        );
        assert_eq!(
            check_link_target("/etc", 3, true).unwrap_err().code,
            ErrorCode::OutsideRoot
        );
        assert!(check_link_target("sibling", 0, false).is_ok());
        assert!(check_link_target("../up", 1, false).is_ok());
        assert_eq!(
            check_link_target("../up", 0, false).unwrap_err().code,
            ErrorCode::OutsideRoot
        );
        assert_eq!(
            check_link_target("a/../../up", 0, false).unwrap_err().code,
            ErrorCode::OutsideRoot
        );
        #[cfg(unix)]
        assert_eq!(
            check_link_target("/etc", 3, false).unwrap_err().code,
            ErrorCode::OutsideRoot
        );
        #[cfg(windows)]
        assert_eq!(
            check_link_target("C:\\Windows", 3, false).unwrap_err().code,
            ErrorCode::OutsideRoot
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_roots_compare_without_case_and_accept_either_separator() {
        let list = roots(&["C:\\Data"]);
        let found = locate(&list, "c:/DATA/Sub\\file.txt").unwrap();
        assert_eq!(found.relative, PathBuf::from("Sub\\file.txt"));
        assert_eq!(
            code(locate(&list, "C:\\DataBase\\x")),
            ErrorCode::OutsideRoot
        );
        assert_eq!(code(locate(&list, "D:\\Data\\x")), ErrorCode::OutsideRoot);
        assert_eq!(
            code(locate(&list, "C:\\Data\\..\\Windows")),
            ErrorCode::OutsideRoot
        );
        assert_eq!(
            locate(&roots(&["\\\\?\\C:\\Data"]), "c:\\data\\x")
                .unwrap()
                .relative,
            PathBuf::from("x")
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_unc_roots_match_by_server_and_share() {
        let list = roots(&["\\\\Server\\Share\\dir"]);
        assert_eq!(
            locate(&list, "\\\\server\\SHARE\\DIR\\f").unwrap().relative,
            PathBuf::from("f")
        );
        assert_eq!(
            code(locate(&list, "\\\\server\\other\\dir\\f")),
            ErrorCode::OutsideRoot
        );
        assert_eq!(
            code(locate(&list, "\\\\server\\share\\elsewhere\\f")),
            ErrorCode::OutsideRoot
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_devices_streams_and_drive_relative_forms_are_refused() {
        let list = roots(&["C:\\Data"]);
        for text in [
            "nul",
            "C:\\Data\\CON.txt",
            "C:\\Data\\com1",
            "f.txt:stream",
            "C:\\Data\\f.txt::$DATA",
            "name.",
            "\\\\.\\C:\\Data\\x",
            "\\\\?\\GLOBALROOT\\x",
            "C:rel",
        ] {
            assert_eq!(code(locate(&list, text)), ErrorCode::BadRequest, "{text}");
        }
    }
}

#[cfg(test)]
mod posix_tests {
    use super::*;

    fn roots(list: &[&str]) -> Vec<PathBuf> {
        list.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn a_posix_path_is_split_from_its_root_whatever_this_system_is() {
        let list = roots(&["/srv/data", "/srv/data/deep"]);
        let found = locate_in(&list, "/srv/data/a//b/./c", Syntax::Posix).unwrap();
        assert_eq!(
            (found.root, found.relative),
            (0, PathBuf::from_iter(["a", "b", "c"]))
        );
        assert_eq!(
            locate_in(&list, "/srv/data/deep/x", Syntax::Posix)
                .unwrap()
                .root,
            1
        );
        assert_eq!(
            locate_in(&roots(&["/srv/data"]), "/srv/data", Syntax::Posix)
                .unwrap()
                .relative,
            PathBuf::new()
        );
    }

    #[test]
    fn a_posix_path_outside_every_root_or_leaving_its_start_is_refused() {
        let list = roots(&["/srv/data"]);
        for text in [
            "/srv/database/x",
            "/etc/passwd",
            "/srv/data/../etc",
            "../x",
            "a/../../x",
        ] {
            let error = locate_in(&list, text, Syntax::Posix).expect_err(text);
            assert_eq!(error.code, ErrorCode::OutsideRoot, "{text}");
        }
        assert_eq!(
            locate_in(&list, "", Syntax::Posix).expect_err("empty").code,
            ErrorCode::BadRequest
        );
    }

    #[test]
    fn posix_names_compare_as_they_are_and_a_drive_is_just_a_name() {
        let list = roots(&["/Srv/Data"]);
        assert!(locate_in(&list, "/srv/data/x", Syntax::Posix).is_err());
        assert!(locate_in(&list, "/Srv/Data/x", Syntax::Posix).is_ok());
        // `/c/Users` is a directory below the root `/c`, not a drive.
        let found = locate_in(&roots(&["/c"]), "/c/Users/x", Syntax::Posix).unwrap();
        assert_eq!(found.relative, PathBuf::from_iter(["Users", "x"]));
        // A name a Windows system refuses is a name here.
        assert!(locate_in(&roots(&["/c"]), "/c/CON:x/y", Syntax::Posix).is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn the_host_syntax_reads_a_drive_and_the_posix_one_does_not() {
        let list = roots(&[r"C:\data"]);
        assert!(locate_in(&list, r"C:\data\x", Syntax::Host).is_ok());
        // In POSIX a backslash is part of a name, which a Windows node cannot carry as one: it would be split into
        // components (`..\..\x` would climb out) when the name is made a path.
        assert_eq!(
            locate_in(&list, r"C:\data\x", Syntax::Posix)
                .unwrap_err()
                .code,
            ErrorCode::BadRequest
        );
        assert!(locate_in(&list, r"/srv/..\..\x", Syntax::Posix).is_err());
    }
}
