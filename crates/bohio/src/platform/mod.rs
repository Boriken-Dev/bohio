//! Operating-system calls: looking for programs on a search path.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Looks for `program` in the directories of `path`, as a shell would. On Windows `.exe` is tried
/// when the name has no extension.
pub fn find_program(program: &str, path: &OsStr) -> Option<PathBuf> {
    if program.is_empty() {
        return None;
    }
    let names: Vec<String> = if cfg!(windows) && Path::new(program).extension().is_none() {
        vec![format!("{program}.exe"), program.to_owned()]
    } else {
        vec![program.to_owned()]
    };
    std::env::split_paths(path)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}
