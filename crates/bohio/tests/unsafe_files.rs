//! The files that may hold `unsafe` code are the ones listed here, each with what needed it and what
//! was ruled out. Any other file that holds it fails the test, and so does a listed file that no
//! longer does: the list is the review surface, so it stays as narrow as it says it is.

use std::fs;
use std::path::{Path, PathBuf};

/// Workspace-relative path, what needed `unsafe` and what was ruled out.
const ALLOWED: &[(&str, &str)] = &[(
    "crates/bohio/src/platform/winproc.rs",
    "starting a process on Windows with a handle list (CreateProcessW, STARTUPINFOEX, \
     PROC_THREAD_ATTRIBUTE_HANDLE_LIST), so a child holds only its three standard streams: std's \
     Command inherits every inheritable handle, and TcpStream::try_clone makes inheritable sockets, \
     which a killed child resets. std offers the attribute only on nightly (raw_attribute), \
     portable-pty starts its child without inheriting handles but cannot start a pipe-only process, \
     and no maintained crate wraps a handle list over pipes",
)];

/// Whether `source` holds an `unsafe` block, function, impl or trait outside comments and strings of
/// one line; `unsafe_code` (the lint) is not one.
fn holds_unsafe(source: &str) -> bool {
    source.lines().any(|line| {
        let code = line.split("//").next().unwrap_or("");
        code.match_indices("unsafe").any(|(at, _)| {
            let before = code[..at].chars().next_back();
            let after = code[at + "unsafe".len()..].chars().next();
            !before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '"')
                && !after.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '"')
        })
    })
}

fn rust_files(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name != "target" && name != ".git" {
                rust_files(&path, found);
            }
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

#[test]
fn the_detector_sees_unsafe_and_nothing_else() {
    assert!(holds_unsafe("fn f() { unsafe { g() } }"));
    assert!(holds_unsafe("unsafe fn f() {}"));
    assert!(holds_unsafe("    unsafe impl Send for X {}"));
    assert!(!holds_unsafe("#![forbid(unsafe_code)]"));
    assert!(!holds_unsafe("// an unsafe block would go here"));
    assert!(!holds_unsafe("let a = \"unsafe\";"));
    assert!(!holds_unsafe("fn not_unsafe_at_all() {}"));
}

#[test]
fn only_the_listed_files_hold_unsafe_code() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    assert!(!files.is_empty(), "no source file was found");
    let this = Path::new(file!()).file_name().expect("a file name");
    let mut holding = Vec::new();
    for file in files {
        if file.file_name() == Some(this) {
            continue;
        }
        let source = fs::read_to_string(&file).expect("a source file");
        if holds_unsafe(&source) {
            let relative = file
                .strip_prefix(&root)
                .expect("under the workspace")
                .to_string_lossy()
                .replace('\\', "/");
            let relative = relative.trim_start_matches("./").to_owned();
            holding.push(relative);
        }
    }
    holding.sort();
    // Unix-only and Windows-only files are all scanned: the list names a file whatever the platform.
    let listed: Vec<String> = ALLOWED.iter().map(|(file, _)| (*file).to_owned()).collect();
    let unlisted: Vec<_> = holding
        .iter()
        .filter(|file| !listed.contains(file))
        .collect();
    let stale: Vec<_> = listed
        .iter()
        .filter(|file| !holding.contains(file))
        .collect();
    assert!(
        unlisted.is_empty(),
        "these files hold unsafe code and are not in ALLOWED (with the reason it was needed): {unlisted:?}"
    );
    assert!(
        stale.is_empty(),
        "these files are in ALLOWED and no longer hold unsafe code: {stale:?}"
    );
}
