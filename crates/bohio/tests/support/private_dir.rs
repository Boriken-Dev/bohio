//! A temporary directory only the running account may use, whatever the machine's temporary folder grants.
//!
//! The owner check refuses a configuration layer or a data directory that another account may change, and a machine can
//! grant a group rights on its temporary folder (another tool's sandbox does): everything made there inherits them. A test
//! that makes layers or a data directory makes them in a directory made here instead: mode `0700` on Unix, and on Windows
//! an access list of this account, SYSTEM and the administrators alone, with nothing inherited.

#![allow(dead_code)]

use std::path::Path;

/// Narrows `path` so only this account (and, on Windows, SYSTEM and the administrators) can change it or read it.
pub fn narrow(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .expect("a private directory");
    }
    #[cfg(windows)]
    {
        let me = this_account();
        let status = std::process::Command::new("icacls")
            .arg(path)
            .args([
                "/inheritance:r",
                "/grant:r",
                &format!("*{me}:(OI)(CI)F"),
                "*S-1-5-18:(OI)(CI)F",
                "*S-1-5-32-544:(OI)(CI)F",
            ])
            .stdout(std::process::Stdio::null())
            .status()
            .expect("icacls runs");
        assert!(status.success(), "icacls narrowed {}", path.display());
    }
}

/// The security id of the running account, as `whoami /user` writes it.
#[cfg(windows)]
fn this_account() -> String {
    use std::sync::OnceLock;
    static ME: OnceLock<String> = OnceLock::new();
    ME.get_or_init(|| {
        let out = std::process::Command::new("whoami")
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .expect("whoami runs");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .rsplit(',')
            .next()
            .expect("a security id")
            .trim_matches('"')
            .to_owned()
    })
    .clone()
}

/// A new temporary directory that only this account may use.
pub fn private_tempdir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a temporary directory");
    narrow(dir.path());
    dir
}
