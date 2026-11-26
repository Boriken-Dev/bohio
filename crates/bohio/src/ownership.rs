//! Refusing to serve from a configuration file or a data directory that other local users may change.
//!
//! The configuration names the data directory, and the data directory holds the host's key, the
//! keys it trusts and what each may do, so whoever may change either decides who the host serves
//! and holds its identity. On Windows the system location (`%ProgramData%`) lets every local user
//! create files and folders by default, and a folder made there inherits that, so a host started from
//! one would take a stranger's configuration, trusted keys or grants. Unix's system locations are
//! the administrator's, and the user locations are private. The same check holds a provider library, which runs
//! as the host's own account once it is mapped.

#![forbid(unsafe_code)]

use std::path::Path;

/// What is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Guarded {
    /// The configuration file: no other user may change it.
    Config,
    /// A provider library or the directory that holds it: no other user may change it.
    Library,
    /// A provider program or the directory that holds it: no other user may change it.
    Program,
    /// The data directory: no other user may change it or read what is made in it.
    DataDir,
    /// The node's key file: no other user may change or read it.
    Identity,
}

impl Guarded {
    const fn name(self) -> &'static str {
        match self {
            Self::Config => "the configuration file",
            Self::Library => "the provider library",
            Self::Program => "the provider program",
            Self::DataDir => "the data directory",
            Self::Identity => "the identity file",
        }
    }
}

/// What is wrong with a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Another account owns it: whoever that is made it, so nothing in it, `check_access` included, is the
    /// operator's.
    Foreign,
    /// The operator's own path, which other accounts may change (or, for the data directory, read).
    Open,
    /// Who may change it could not be found out.
    Unknown,
}

/// Why a path is refused: its [`Kind`] and one line naming the path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Problem {
    pub(crate) kind: Kind,
    pub(crate) text: String,
    /// For [`Kind::Foreign`], who owns the path: a user id (Unix) or a security id (Windows).
    pub(crate) owner: Option<String>,
}

impl Problem {
    fn new(kind: Kind, text: String) -> Self {
        Self {
            kind,
            text,
            owner: None,
        }
    }

    fn owned_by(mut self, owner: String) -> Self {
        self.owner = Some(owner);
        self
    }
}

/// Refuses `path` when a user other than the host's own account, SYSTEM or the administrators owns it
/// or may change it (for the data directory, also read what is made in it). On Unix a configuration
/// layer or its directory is refused when another account than the running one or root owns it, or its
/// group or others may write it. The data directory is held to the same rule, and its key file must also be
/// closed to the group and others.
///
/// # Errors
///
/// A [`Problem`] saying what is wrong and with which path, or why it could not be checked.
#[cfg(unix)]
pub(crate) fn check(path: &Path, what: Guarded) -> Result<(), Problem> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path).map_err(|error| {
        Problem::new(
            Kind::Unknown,
            format!(
                "cannot check who may change {} {}: {error}",
                what.name(),
                path.display()
            ),
        )
    })?;
    let me = rustix::process::geteuid().as_raw();
    if meta.uid() != me && meta.uid() != 0 {
        return Err(Problem::new(
            Kind::Foreign,
            format!(
                "{} {} is owned by another account: give it to the account that runs bohio, or to root",
                what.name(),
                path.display()
            ),
        )
        .owned_by(meta.uid().to_string()));
    }
    let forbidden = if what == Guarded::Identity {
        0o077
    } else {
        0o022
    };
    if meta.mode() & forbidden != 0 {
        return Err(Problem::new(
            Kind::Open,
            format!(
                "{} {} can be changed (or, for a key, read) by its group or by others: remove their access",
                what.name(),
                path.display()
            ),
        ));
    }
    Ok(())
}

/// Nothing is checked where the platform has neither Unix modes nor Windows access control lists.
///
/// # Errors
///
/// Never.
#[cfg(not(any(unix, windows)))]
pub(crate) fn check(_path: &Path, _what: Guarded) -> Result<(), Problem> {
    Ok(())
}

/// As the other platforms' `check`, through the access control list Windows keeps for `path`.
///
/// # Errors
///
/// A [`Problem`] saying what is wrong and with which path, or why it could not be checked.
#[cfg(windows)]
pub(crate) fn check(path: &Path, what: Guarded) -> Result<(), Problem> {
    let listed = windows::list(path).map_err(|reason| {
        Problem::new(
            Kind::Unknown,
            format!(
                "cannot check who may change {} {}: {reason}",
                what.name(),
                path.display()
            ),
        )
    })?;
    match windows::judge(&listed, what) {
        Ok(()) => Ok(()),
        Err((kind, who, owner)) => {
            let problem = Problem::new(
                kind,
                format!(
                    "{} {} {who}: give it to the account that runs bohio, SYSTEM and the administrators alone",
                    what.name(),
                    path.display()
                ),
            );
            Err(match owner {
                Some(owner) => problem.owned_by(owner),
                None => problem,
            })
        }
    }
}

/// The problems of `layers` (each file and the directory that holds it) as `check_access` decides them: a path
/// another account owns is refused whatever the setting says, since that account could have written the
/// setting; the rest are refused while `check_access` holds, and otherwise logged.
///
/// # Errors
///
/// Every refused problem, joined into one line.
pub(crate) fn decide(
    problems: &[Problem],
    check_access: bool,
    trusted_owners: &[String],
) -> Result<(), String> {
    let (refused, waived): (Vec<&Problem>, Vec<&Problem>) = problems.iter().partition(|problem| {
        if problem.kind == Kind::Foreign {
            !problem
                .owner
                .as_deref()
                .is_some_and(|owner| trusts(trusted_owners, owner))
        } else {
            check_access
        }
    });
    for problem in waived {
        if problem.kind == Kind::Foreign {
            tracing::warn!(
                "non-default: an account in `trusted_owners` (or `--trust-owner`) owns a path that decides who connects: {}",
                problem.text
            );
        } else {
            tracing::warn!("going on, as `check_access` is false: {}", problem.text);
        }
    }
    if refused.is_empty() {
        Ok(())
    } else {
        let texts: Vec<&str> = refused
            .iter()
            .map(|problem| problem.text.as_str())
            .collect();
        Err(texts.join("; "))
    }
}

/// Whether `owner` (a user id on Unix, a security id on Windows) is one of `trusted`: the same id, or on Unix an
/// account name whose id it is.
fn trusts(trusted: &[String], owner: &str) -> bool {
    trusted.iter().any(|entry| {
        let entry = entry.trim();
        entry.eq_ignore_ascii_case(owner) || user_id_of(entry).is_some_and(|id| id == owner)
    })
}

/// The id `/etc/passwd` gives the account `name`.
#[cfg(unix)]
fn user_id_of(name: &str) -> Option<String> {
    let table = std::fs::read_to_string("/etc/passwd").ok()?;
    table.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next() == Some(name)).then(|| fields.nth(1).map(str::to_owned))?
    })
}

/// Windows entries are security ids.
#[cfg(not(unix))]
fn user_id_of(_name: &str) -> Option<String> {
    None
}

/// The accounts the operator trusts on the command line or in `BOHIO_TRUST_OWNER` (comma separated): the only
/// source for the lowest layer itself, which cannot vouch for its own owner.
pub(crate) fn trusted_from_environment() -> Vec<String> {
    std::env::var("BOHIO_TRUST_OWNER")
        .map(|value| {
            value
                .split(',')
                .map(|entry| entry.trim().to_owned())
                .filter(|entry| !entry.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

/// The problems of the layer file `file` and of the directory that holds it.
pub(crate) fn layer_problems(file: &Path) -> Vec<Problem> {
    let mut problems = Vec::new();
    if let Err(problem) = check(file, Guarded::Config) {
        problems.push(problem);
    }
    if let Err(problem) = check(&directory_of(file), Guarded::Config) {
        problems.push(problem);
    }
    problems.extend(target_problems(file, Guarded::Config));
    problems
}

/// What [`library_problems`] already found out about directories, so one that holds several libraries is checked once
/// (on Windows a check reads the access list through the security API).
pub(crate) type Checked = std::collections::HashMap<std::path::PathBuf, Option<Problem>>;

/// The problems of the provider library `file` and of the directory that holds it, `checked` remembering directories.
pub(crate) fn library_problems(file: &Path, checked: &mut Checked) -> Vec<Problem> {
    held_problems(file, checked, Guarded::Library)
}

/// The problems of the provider program `file` and of the directory that holds it, `checked` remembering directories.
pub(crate) fn program_problems(file: &Path, checked: &mut Checked) -> Vec<Problem> {
    held_problems(file, checked, Guarded::Program)
}

fn held_problems(file: &Path, checked: &mut Checked, what: Guarded) -> Vec<Problem> {
    let directory = directory_of(file);
    let held = checked
        .entry(directory.clone())
        .or_insert_with(|| check(&directory, what).err())
        .clone();
    check(file, what)
        .err()
        .into_iter()
        .chain(held)
        .chain(target_problems(file, what))
        .collect()
}

/// When `file` is a link, the problems of the file it leads to and of the directory that holds that file: whoever may
/// write there can replace what the link names.
fn target_problems(file: &Path, what: Guarded) -> Vec<Problem> {
    if !std::fs::symlink_metadata(file).is_ok_and(|meta| meta.file_type().is_symlink()) {
        return Vec::new();
    }
    let Ok(target) = std::fs::canonicalize(file) else {
        return Vec::new();
    };
    let mut problems: Vec<Problem> = check(&target, what).err().into_iter().collect();
    problems.extend(check(&directory_of(&target), what).err());
    problems
}

/// The directory that holds `path`: its parent, or the current directory for a bare file name.
fn directory_of(path: &Path) -> std::path::PathBuf {
    path.parent()
        .filter(|directory| !directory.as_os_str().is_empty())
        .map_or_else(|| Path::new(".").to_owned(), Path::to_owned)
}

/// Whether a layer `file` that does not exist yet may be read once it does: the nearest directory above it that
/// exists must be one no other account may change, or another account could make the file and choose the
/// peers. `Err` says why not.
pub(crate) fn may_appear(file: &Path) -> Result<(), Problem> {
    let directory = directory_of(file);
    let existing = directory
        .ancestors()
        .find(|ancestor| !ancestor.as_os_str().is_empty() && ancestor.is_dir())
        .map_or_else(|| Path::new(".").to_owned(), Path::to_owned);
    check(&existing, Guarded::Config)
}

#[cfg(any(windows, test))]
mod windows {
    use super::{Guarded, Kind};

    /// SYSTEM, the administrators and TrustedInstaller, who may change anything anyway.
    const TRUSTED: [&str; 3] = [
        "S-1-5-18",
        "S-1-5-32-544",
        "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464",
    ];

    /// Groups every local or signed-in user is in.
    const EVERYONE: [&str; 10] = [
        "S-1-1-0",      // Everyone
        "S-1-5-2",      // NETWORK
        "S-1-5-4",      // INTERACTIVE
        "S-1-5-7",      // ANONYMOUS LOGON
        "S-1-5-11",     // Authenticated Users
        "S-1-5-15",     // This Organization
        "S-1-5-32-545", // Users
        "S-1-5-32-546", // Guests
        "S-1-5-113",    // Local account
        "S-1-5-114",    // Local account and member of Administrators
    ];

    /// CREATOR OWNER: an entry for it applies to whoever makes a child, the host's own account here.
    const CREATOR_OWNER: &str = "S-1-3-0";

    /// Rights that change an object or what it holds: write data or add a file, append or add a folder,
    /// write extended attributes, delete a child, write attributes, delete, change the access list,
    /// take ownership, and the generic all and write.
    const CHANGE: u32 = 0x2
        | 0x4
        | 0x10
        | 0x40
        | 0x100
        | 0x1_0000
        | 0x4_0000
        | 0x8_0000
        | 0x1000_0000
        | 0x4000_0000;

    /// Rights that read what an object holds: read data or list, and the generic all and read.
    const READ: u32 = 0x1 | 0x1000_0000 | 0x8000_0000;

    /// `PropagationFlags.InheritOnly`: the entry applies to children, not to the object itself.
    const INHERIT_ONLY: u32 = 0x2;

    /// What one access control list says.
    #[derive(Debug, Default, PartialEq, Eq)]
    pub(super) struct Listed {
        pub(super) me: String,
        pub(super) owner: String,
        /// Allow entries: the account, the rights, the propagation flags.
        pub(super) allowed: Vec<(String, u32, u32)>,
    }

    /// Why `listed` is not safe for `what`: who owns it, or who may change or read it.
    pub(super) fn judge(
        listed: &Listed,
        what: Guarded,
    ) -> Result<(), (Kind, String, Option<String>)> {
        let trusted = |sid: &str| TRUSTED.contains(&sid) || sid == listed.me;
        if !trusted(&listed.owner) {
            return Err((
                Kind::Foreign,
                format!("is owned by another account ({})", listed.owner),
                Some(listed.owner.clone()),
            ));
        }
        for (sid, rights, propagation) in &listed.allowed {
            if trusted(sid) || sid == CREATOR_OWNER {
                continue;
            }
            let broad = EVERYONE.contains(&sid.as_str())
                || (sid.starts_with("S-1-5-21-")
                    && (sid.ends_with("-513") || sid.ends_with("-514")));
            if !broad {
                // Any other account or group that may change it decides what the node reads, as much as every user would.
                let on_object = propagation & INHERIT_ONLY == 0;
                let changes = match what {
                    Guarded::Config | Guarded::Library | Guarded::Program => {
                        on_object && rights & CHANGE != 0
                    }
                    Guarded::DataDir | Guarded::Identity => rights & CHANGE != 0,
                };
                if changes {
                    return Err((
                        Kind::Open,
                        format!("may be changed by another account ({sid})"),
                        None,
                    ));
                }
                continue;
            }
            // An entry that applies only to children still reaches the files the host makes there.
            let on_object = propagation & INHERIT_ONLY == 0;
            let reaches = match what {
                Guarded::Config | Guarded::Library | Guarded::Program => {
                    on_object && rights & CHANGE != 0
                }
                Guarded::DataDir | Guarded::Identity => rights & (CHANGE | READ) != 0,
            };
            if reaches {
                return Err((Kind::Open, format!("is open to every user ({sid})"), None));
            }
        }
        Ok(())
    }

    /// The owner and the allow entries of `path`, and this process's account, as security ids.
    #[cfg(windows)]
    pub(super) fn list(path: &std::path::Path) -> Result<Listed, String> {
        let acl = crate::platform::winacl::read(path)?;
        Ok(Listed {
            me: acl.me,
            owner: acl.owner,
            allowed: acl.allowed,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Guarded;
    use super::windows::judge;

    use super::windows::Listed;

    /// Reads `me|`, `owner|` and `ace|` lines into a list.
    fn parse(text: &str) -> Result<Listed, String> {
        let mut listed = Listed::default();
        for line in text.lines().map(str::trim).filter(|line| !line.is_empty()) {
            let fields: Vec<&str> = line.split('|').collect();
            match fields.as_slice() {
                ["me", sid] => listed.me = (*sid).to_owned(),
                ["owner", sid] => listed.owner = (*sid).to_owned(),
                ["ace", sid, "Allow", rights, propagation] => {
                    let rights = u32::from_str_radix(rights, 16)
                        .map_err(|_| format!("an access right that is not hex: {rights}"))?;
                    let propagation = propagation.parse::<u32>().map_err(|_| {
                        format!("propagation flags that are not a number: {propagation}")
                    })?;
                    listed
                        .allowed
                        .push(((*sid).to_owned(), rights, propagation));
                }
                ["ace", _, _, _, _] => {}
                _ => return Err(format!("an unexpected line: {line}")),
            }
        }
        if listed.me.is_empty() || listed.owner.is_empty() {
            return Err("the owner or this account was not reported".to_owned());
        }
        Ok(listed)
    }

    const ME: &str = "S-1-5-21-1-2-3-1001";

    fn listed(owner: &str, aces: &[&str]) -> super::windows::Listed {
        let mut text = format!("me|{ME}\nowner|{owner}\n");
        for ace in aces {
            text.push_str(ace);
            text.push('\n');
        }
        parse(&text).expect("the lines parse")
    }

    #[test]
    fn a_private_directory_passes_and_program_data_does_not() {
        let private = listed(
            ME,
            &[
                "ace|S-1-5-18|Allow|1F01FF|0",
                "ace|S-1-5-32-544|Allow|1F01FF|0",
                &format!("ace|{ME}|Allow|1F01FF|0"),
                "ace|S-1-5-21-9-9-9-1234|Allow|1200A9|0",
            ],
        );
        assert!(judge(&private, Guarded::DataDir).is_ok());
        assert!(judge(&private, Guarded::Config).is_ok());
        // What a folder under %ProgramData% inherits: every user reads it and may add to it.
        let program_data = listed(
            "S-1-5-32-544",
            &[
                "ace|S-1-5-18|Allow|1F01FF|0",
                "ace|S-1-3-0|Allow|10000000|2",
                "ace|S-1-5-32-545|Allow|1200A9|0",
                "ace|S-1-5-32-545|Allow|116|0",
            ],
        );
        assert!(judge(&program_data, Guarded::DataDir).is_err());
        assert!(judge(&program_data, Guarded::Config).is_err());
    }

    #[test]
    fn a_file_another_user_made_or_every_user_may_change_is_refused() {
        let planted = listed("S-1-5-21-1-2-3-1002", &["ace|S-1-5-18|Allow|1F01FF|0"]);
        assert!(judge(&planted, Guarded::Config).is_err());
        let readable = listed(ME, &["ace|S-1-5-32-545|Allow|1200A9|0"]);
        assert!(
            judge(&readable, Guarded::Config).is_ok(),
            "reading a configuration is harmless"
        );
        assert!(
            judge(&readable, Guarded::DataDir).is_err(),
            "the key would be readable"
        );
        let domain = listed(ME, &["ace|S-1-5-21-7-8-9-513|Allow|116|0"]);
        assert!(judge(&domain, Guarded::Config).is_err());
        let children = listed(ME, &["ace|S-1-1-0|Allow|80000000|2"]);
        assert!(judge(&children, Guarded::Config).is_ok());
        assert!(judge(&children, Guarded::DataDir).is_err());
        let denied = listed(ME, &["ace|S-1-1-0|Deny|1F01FF|0"]);
        assert!(judge(&denied, Guarded::DataDir).is_ok());
    }

    /// Only the owner, SYSTEM, the administrators and TrustedInstaller may write: one other named account may not either.
    #[test]
    fn a_named_account_that_may_write_is_refused_and_one_that_may_read_is_not() {
        let writer = listed(ME, &["ace|S-1-5-21-1-2-3-1002|Allow|2|0"]);
        for what in [
            Guarded::Config,
            Guarded::Library,
            Guarded::Program,
            Guarded::DataDir,
        ] {
            assert!(judge(&writer, what).is_err(), "{what:?}");
        }
        let group = listed(ME, &["ace|S-1-5-32-547|Allow|1F01FF|0"]);
        assert!(judge(&group, Guarded::Config).is_err(), "Power Users");
        let reader = listed(ME, &["ace|S-1-5-21-1-2-3-1002|Allow|1200A9|0"]);
        assert!(judge(&reader, Guarded::Config).is_ok());
        let children = listed(ME, &["ace|S-1-5-21-1-2-3-1002|Allow|2|2"]);
        assert!(judge(&children, Guarded::Config).is_ok(), "children only");
        assert!(judge(&children, Guarded::DataDir).is_err());
    }

    #[test]
    fn output_that_is_not_an_access_list_is_an_error() {
        assert!(parse("owner|S-1-5-18\n").is_err());
        assert!(parse("me|x\nowner|y\nace|z|Allow|nothex|0\n").is_err());
        assert!(parse("me|x\nowner|y\nsomething else\n").is_err());
    }
}

#[cfg(test)]
mod trust_tests {
    use super::{Kind, Problem, decide};

    fn foreign(owner: &str) -> Problem {
        Problem {
            kind: Kind::Foreign,
            text: "the configuration file is owned by another account".to_owned(),
            owner: Some(owner.to_owned()),
        }
    }

    #[test]
    fn a_path_a_trusted_account_owns_is_served_and_another_accounts_is_not() {
        let trusted = ["1234".to_owned()];
        assert!(decide(&[foreign("1234")], true, &trusted).is_ok());
        assert!(decide(&[foreign("1235")], true, &trusted).is_err());
        assert!(decide(&[foreign("1234")], true, &[]).is_err());
        // Ownership trusted is not the write check waived.
        let open = Problem {
            kind: Kind::Open,
            text: "open".to_owned(),
            owner: None,
        };
        assert!(decide(&[foreign("1234"), open], true, &trusted).is_err());
    }
}

#[cfg(all(test, unix))]
mod link_tests {
    use super::{Kind, layer_problems};
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &std::path::Path, bits: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits)).expect("mode");
    }

    /// The link's own directory is private, but the file it names can be renamed by anyone who may write its directory.
    #[test]
    fn a_link_into_a_directory_others_may_write_is_refused() {
        let root = tempfile::tempdir().expect("a directory");
        let mine = root.path().join("mine");
        let open = root.path().join("open");
        std::fs::create_dir(&mine).expect("mine");
        std::fs::create_dir(&open).expect("open");
        mode(&mine, 0o700);
        let real = open.join("bohio.cfg");
        std::fs::write(&real, "").expect("the file");
        mode(&real, 0o600);
        mode(&open, 0o777);
        let link = mine.join("bohio.cfg");
        std::os::unix::fs::symlink(&real, &link).expect("the link");
        let problems = layer_problems(&link);
        assert!(
            problems.iter().any(|problem| problem.kind == Kind::Open),
            "{problems:?}"
        );
        mode(&open, 0o700);
        assert!(layer_problems(&link).is_empty());
    }
}

/// The real access lists of files this test makes, read through the security API.
#[cfg(all(test, windows))]
mod real_tests {
    use super::{Guarded, Kind, check};
    use std::path::Path;

    fn icacls(path: &Path, args: &[&str]) {
        let status = std::process::Command::new("icacls")
            .arg(path)
            .args(args)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("icacls runs");
        assert!(status.success(), "icacls {args:?}");
    }

    #[test]
    fn the_security_api_reads_the_lists_icacls_writes() {
        // `icacls` is a child of this process: not while a test holds connections open.
        let _quiet = crate::platform::SPAWN_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = tempfile::tempdir().expect("a directory");
        // A temporary directory inherits whatever groups the machine gives %TEMP% (a sandbox's, for one), so this one is
        // narrowed to this account, SYSTEM and the administrators before a file is made in it.
        crate::private_dir::narrow(directory.path());
        let file = directory.path().join("layer.cfg");
        std::fs::write(&file, "# layer\n").expect("a file");
        let listed = crate::platform::winacl::read(&file).expect("the list");
        assert_eq!(listed.owner, listed.me, "this account made it");
        assert!(listed.me.starts_with("S-1-5-"), "{}", listed.me);
        assert!(
            listed.allowed.iter().any(|(sid, _, _)| sid == "S-1-5-18"),
            "SYSTEM is in a user's temporary directory: {listed:?}"
        );
        check(&file, Guarded::Config).expect("a file this account made passes");

        // Another named group that may write is refused, as the owner decided; one that may only read is not.
        icacls(&file, &["/grant", "*S-1-5-32-551:(R)"]);
        check(&file, Guarded::Config).expect("a reader is harmless");
        icacls(&file, &["/grant", "*S-1-5-32-551:(W)"]);
        let refused = check(&file, Guarded::Config).expect_err("a group that may write");
        assert_eq!(refused.kind, Kind::Open, "{}", refused.text);
        assert!(refused.text.contains("S-1-5-32-551"), "{}", refused.text);
        icacls(&file, &["/remove:g", "*S-1-5-32-551"]);
        check(&file, Guarded::Config).expect("passes again");

        // Everyone with write, and a directory the same way.
        icacls(&file, &["/grant", "*S-1-1-0:(M)"]);
        let refused = check(&file, Guarded::Config).expect_err("every user may change it");
        assert_eq!(refused.kind, Kind::Open, "{}", refused.text);
        let missing = directory.path().join("absent.cfg");
        assert!(crate::platform::winacl::read(&missing).is_err());
    }
}
