//! The `bohio` command's way of reading the secret references of `bohio.cfg`: `file:PATH` and `credential:NAME`.
//!
//! A file is refused unless only its owner (and the administrators) may read it, the check the configuration layers get
//! made on what they hold. `credential:NAME` is a file of the directory the service manager passes in
//! (`$CREDENTIALS_DIRECTORY`, systemd's `LoadCredential`), whose name may not leave it. Neither names the secret in an
//! error or a log.

use std::io::Read;
use std::path::{Path, PathBuf};

use crate::secret::{Secret, SecretError, SecretResolver, split};

/// Reads references as the command does.
#[derive(Debug, Clone, Default)]
pub(super) struct CommandSecrets {
    /// `$CREDENTIALS_DIRECTORY`, read by the command.
    credentials: Option<PathBuf>,
}

impl CommandSecrets {
    /// A resolver whose `credential:` references are files of `credentials`.
    pub(super) fn new(credentials: Option<PathBuf>) -> Self {
        Self { credentials }
    }

    /// One taken from the process environment.
    pub(super) fn from_environment() -> Self {
        Self::new(
            std::env::var_os("CREDENTIALS_DIRECTORY")
                .filter(|dir| !dir.is_empty())
                .map(PathBuf::from),
        )
    }
}

fn open(path: &Path, what: &str) -> Result<std::fs::File, SecretError> {
    std::fs::File::open(path)
        .map_err(|error| SecretError::new(format!("{what} cannot be opened: {}", error.kind())))
}

fn read_bounded(file: std::fs::File, max_bytes: usize, what: &str) -> Result<Secret, SecretError> {
    let mut bytes = Vec::new();
    file.take(
        u64::try_from(max_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1),
    )
    .read_to_end(&mut bytes)
    .map_err(|error| SecretError::new(format!("{what} cannot be read: {}", error.kind())))?;
    if bytes.len() > max_bytes {
        return Err(SecretError::new(format!(
            "{what} is larger than [secrets] max_bytes"
        )));
    }
    let text =
        String::from_utf8(bytes).map_err(|_| SecretError::new(format!("{what} is not text")))?;
    // A file ends with the newline an editor adds; the secret does not.
    Ok(Secret::new(text.trim_end_matches(['\r', '\n'])))
}

/// Whether the open `file` is the node's own account's and closed to the group and others. The answer is about the file that is
/// read, not about a path that can be swapped between the check and the read.
#[cfg(unix)]
fn only_the_owner(file: &std::fs::File, _path: &Path) -> Result<(), SecretError> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata().map_err(|error| {
        SecretError::new(format!(
            "the secret file cannot be looked at: {}",
            error.kind()
        ))
    })?;
    if meta.uid() != rustix::process::geteuid().as_raw() {
        return Err(SecretError::new(
            "the secret file belongs to another account",
        ));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(SecretError::new(
            "the secret file may be read by other accounts: make it readable by its owner alone (chmod 600)",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn only_the_owner(_file: &std::fs::File, path: &Path) -> Result<(), SecretError> {
    crate::ownership::check(path, crate::ownership::Guarded::DataDir)
        .map_err(|_| SecretError::new("the secret file may be read or changed by other accounts"))
}

#[cfg(not(any(unix, windows)))]
fn only_the_owner(_file: &std::fs::File, _path: &Path) -> Result<(), SecretError> {
    Ok(())
}

impl SecretResolver for CommandSecrets {
    fn resolve(&self, reference: &str, max_bytes: usize) -> Result<Secret, SecretError> {
        match split(reference) {
            Some(("file", path)) => {
                let path = Path::new(path);
                let file = open(path, "the secret file")?;
                only_the_owner(&file, path)?;
                read_bounded(file, max_bytes, "the secret file")
            }
            Some(("credential", name)) => {
                let dir = self.credentials.as_deref().ok_or_else(|| {
                    SecretError::new("no credentials directory was passed to this command")
                })?;
                let plain = Path::new(name).components().count() == 1
                    && !name.contains(['/', '\\'])
                    && name != "..";
                if !plain {
                    return Err(SecretError::new(
                        "a credential is named by a file name that stays in the credentials directory",
                    ));
                }
                read_bounded(
                    open(&dir.join(name), "the credential")?,
                    max_bytes,
                    "the credential",
                )
            }
            Some(("keyring", _)) => Err(SecretError::new(
                "this build has no keyring: use file: or credential:",
            )),
            _ => Err(SecretError::new("that is not a reference to a secret")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn a_file_other_accounts_may_read_is_refused_and_an_owner_only_one_is_read() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("a directory");
        let file = dir.path().join("pw");
        std::fs::write(
            &file, "hunter2
",
        )
        .expect("written");
        let reference = format!("file:{}", file.display());
        let secrets = CommandSecrets::default();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).expect("mode");
        let error = secrets.resolve(&reference, 64).expect_err("world-readable");
        assert!(error.to_string().contains("other accounts"), "{error}");
        assert!(!error.to_string().contains("hunter2"));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).expect("mode");
        assert_eq!(
            secrets
                .resolve(&reference, 64)
                .expect("owner only")
                .expose(),
            "hunter2"
        );
    }

    /// The check is about the file that was opened: a different file put at the path afterwards is not what was judged.
    #[cfg(unix)]
    #[test]
    fn the_owner_check_judges_the_open_file_and_not_the_path() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("a directory");
        let path = dir.path().join("pw");
        std::fs::write(&path, "kept\n").expect("written");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("mode");
        let opened = open(&path, "the secret file").expect("opened");
        std::fs::rename(&path, dir.path().join("moved")).expect("moved");
        std::fs::write(&path, "swapped\n").expect("a swapped file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("mode");
        assert!(only_the_owner(&opened, &path).is_ok());
        assert!(only_the_owner(&open(&path, "the secret file").unwrap(), &path).is_err());
    }

    #[test]
    fn a_credential_stays_in_its_directory_and_a_missing_one_names_no_path() {
        let dir = tempfile::tempdir().expect("a directory");
        std::fs::write(dir.path().join("pw"), "hunter2\n").expect("written");
        let secrets = CommandSecrets::new(Some(dir.path().to_path_buf()));
        assert_eq!(
            secrets.resolve("credential:pw", 64).expect("read").expose(),
            "hunter2"
        );
        for bad in ["credential:../pw", "credential:a/b"] {
            assert!(secrets.resolve(bad, 64).is_err(), "{bad}");
        }
        let error = secrets.resolve("credential:nope", 64).expect_err("missing");
        assert!(
            !error
                .to_string()
                .contains(&dir.path().to_string_lossy().into_owned())
        );
        assert!(
            secrets.resolve("credential:pw", 3).is_err(),
            "bounded by max_bytes"
        );
        assert!(
            CommandSecrets::default()
                .resolve("credential:pw", 64)
                .is_err()
        );
    }
}
