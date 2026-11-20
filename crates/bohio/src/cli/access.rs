//! Refusing to serve from a configuration file or a data directory that other local users may change.
//!
//! The configuration names the data directory, and the data directory holds the host's key, the
//! keys it trusts and what each may do, so whoever may change either decides who the host serves
//! and holds its identity. On Windows the system location (`%ProgramData%`) lets every local user
//! create files and folders by default, and a folder made there inherits that, so a host started from
//! one would take a stranger's configuration, trusted keys or grants. Unix's system locations are
//! the administrator's, and the user locations are private.

use std::path::Path;

/// What is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Guarded {
    /// The configuration file: no other user may change it.
    Config,
    /// The data directory: no other user may change it or read what is made in it.
    DataDir,
}

#[cfg(windows)]
impl Guarded {
    const fn name(self) -> &'static str {
        match self {
            Self::Config => "the configuration file",
            Self::DataDir => "the data directory",
        }
    }
}

/// Refuses `path` when a user other than the host's own account, SYSTEM or the administrators owns it
/// or may change it (for the data directory, also read what is made in it). On Unix a configuration
/// layer or its directory is refused when another account than the running one or root owns it, or its
/// group or others may write it; the data directory is not checked there.
///
/// # Errors
///
/// One line saying what is wrong and with which path, or why it could not be checked.
#[cfg(unix)]
pub(super) fn check(path: &Path, what: Guarded) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    if what != Guarded::Config {
        return Ok(());
    }
    let meta = std::fs::metadata(path).map_err(|error| {
        format!(
            "cannot check who may change the configuration {}: {error}",
            path.display()
        )
    })?;
    let me = rustix::process::geteuid().as_raw();
    if meta.uid() != me && meta.uid() != 0 {
        return Err(format!(
            "the configuration {} is owned by another account: give it to the account that runs bohio, or to root",
            path.display()
        ));
    }
    if meta.mode() & 0o022 != 0 {
        return Err(format!(
            "the configuration {} can be changed by its group or by others: remove their write access",
            path.display()
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
pub(super) fn check(_path: &Path, _what: Guarded) -> Result<(), String> {
    Ok(())
}

/// As the other platforms' `check`, through the access control list Windows keeps for `path`.
///
/// # Errors
///
/// One line saying what is wrong and with which path, or why it could not be checked.
#[cfg(windows)]
pub(super) fn check(path: &Path, what: Guarded) -> Result<(), String> {
    let listed = windows::list(path).map_err(|reason| {
        format!(
            "cannot check who may change {} {}: {reason}",
            what.name(),
            path.display()
        )
    })?;
    match windows::judge(&listed, what) {
        Ok(()) => Ok(()),
        Err(who) => Err(format!(
            "{} {} {who}: give it to the account that runs bohio, SYSTEM and the administrators alone",
            what.name(),
            path.display()
        )),
    }
}

#[cfg(any(windows, test))]
mod windows {
    use super::Guarded;

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

    /// Reads the output of the PowerShell lines in `list`.
    pub(super) fn parse(text: &str) -> Result<Listed, String> {
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

    /// Why `listed` is not safe for `what`: who owns it, or who may change or read it.
    pub(super) fn judge(listed: &Listed, what: Guarded) -> Result<(), String> {
        let trusted = |sid: &str| TRUSTED.contains(&sid) || sid == listed.me;
        if !trusted(&listed.owner) {
            return Err(format!("is owned by another account ({})", listed.owner));
        }
        for (sid, rights, propagation) in &listed.allowed {
            if trusted(sid) || sid == CREATOR_OWNER {
                continue;
            }
            let broad = EVERYONE.contains(&sid.as_str())
                || (sid.starts_with("S-1-5-21-")
                    && (sid.ends_with("-513") || sid.ends_with("-514")));
            if !broad {
                continue;
            }
            // An entry that applies only to children still reaches the files the host makes there.
            let on_object = propagation & INHERIT_ONLY == 0;
            let reaches = match what {
                Guarded::Config => on_object && rights & CHANGE != 0,
                Guarded::DataDir => rights & (CHANGE | READ) != 0,
            };
            if reaches {
                return Err(format!("is open to every user ({sid})"));
            }
        }
        Ok(())
    }

    /// The owner and the allow entries of `path`, and this process's account, as security ids.
    #[cfg(windows)]
    pub(super) fn list(path: &std::path::Path) -> Result<Listed, String> {
        // The path travels in the environment, never inside the script, so no text of it is run.
        // .NET's calls need no module, so a module path inherited from another PowerShell is not used.
        const SCRIPT: &str = r#"$ErrorActionPreference = 'Stop'
$sid = [System.Security.Principal.SecurityIdentifier]
$path = $env:BOHIO_CHECKED_PATH
if ([System.IO.Directory]::Exists($path)) { $acl = [System.IO.Directory]::GetAccessControl($path) }
else { $acl = [System.IO.File]::GetAccessControl($path) }
"me|" + [System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value
"owner|" + $acl.GetOwner($sid).Value
foreach ($rule in $acl.GetAccessRules($true, $true, $sid)) {
  "ace|{0}|{1}|{2:X}|{3}" -f $rule.IdentityReference.Value, $rule.AccessControlType, $rule.FileSystemRights.value__, $rule.PropagationFlags.value__
}"#;
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let shell = std::path::Path::new(&root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        let output = std::process::Command::new(shell)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                SCRIPT,
            ])
            .env("BOHIO_CHECKED_PATH", path)
            .env_remove("PSModulePath")
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|error| format!("PowerShell could not be started: {error}"))?;
        if !output.status.success() {
            let said = String::from_utf8_lossy(&output.stderr);
            let first = said
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or("");
            return Err(format!(
                "PowerShell could not read its access list: {}",
                first.trim()
            ));
        }
        parse(&String::from_utf8_lossy(&output.stdout))
    }
}

#[cfg(test)]
mod tests {
    use super::Guarded;
    use super::windows::{judge, parse};

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

    #[test]
    fn output_that_is_not_an_access_list_is_an_error() {
        assert!(parse("owner|S-1-5-18\n").is_err());
        assert!(parse("me|x\nowner|y\nace|z|Allow|nothex|0\n").is_err());
        assert!(parse("me|x\nowner|y\nsomething else\n").is_err());
    }
}
