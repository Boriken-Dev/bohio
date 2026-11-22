//! The files that may hold `unsafe` code are the ones listed here, each with what needed it, what
//! was ruled out and whether Miri reaches it. Any other file that holds it fails the test, and so
//! does a listed file that no longer does: the list is the review surface, so it stays as narrow as
//! it says it is.
//!
//! The `bohio` crate denies `unsafe_code` for the whole crate (`[lints.rust]`); every module that
//! needs none forbids it at its top, and a listed file allows it. `platform/mod.rs` only denies,
//! because the listed files are its children.

#![forbid(unsafe_code)]

use std::fs;
use std::path::{Path, PathBuf};

/// One file that holds `unsafe`.
struct Allowed {
    /// Workspace-relative path.
    file: &'static str,
    /// What needed `unsafe`, and what was ruled out.
    reason: &'static str,
    /// Whether Miri runs this file's `unsafe`, and why not when it does not. Miri is run locally
    /// on a pinned nightly (`cargo +nightly miri test`); a file Miri cannot reach is covered by a
    /// native test on each platform it builds for, which the entry names.
    miri: &'static str,
}

const ALLOWED: &[Allowed] = &[
    Allowed {
        file: "crates/bohio/src/platform/guard.rs",
        reason: "ending a program's process group when the host dies without ending it (Unix): a guardian \n             process, made with fork between the child's fork and exec, reads a pipe only the host holds \n             the write end of and signals the group on end-of-file. Ruled out: PR_SET_PDEATHSIG and \n             procctl(PROC_PDEATHSIG_CTL) signal only the child they are set on, never its descendants \n             (measured: the child died and its own child lived on), and macOS has neither; a kqueue \n             watcher or a supervisor process needs the same fork and a program to run it; no maintained \n             crate makes a guardian. std's `pre_exec` is itself unsafe, and rustix offers no fork",
        miri: "not reachable: Miri does not run fork, waitpid or close_range; covered by the host-death \n           tests of tests/link.rs, which SIGKILL `serve` and look for the exec tree, on Linux (and \n           macOS or FreeBSD where run)",
    },
    Allowed {
        file: "crates/bohio/src/platform/winproc.rs",
        reason: "starting a process on Windows with a handle list (CreateProcessW, STARTUPINFOEX, \
             PROC_THREAD_ATTRIBUTE_HANDLE_LIST), so a child holds only its three standard streams: \
             std's Command inherits every inheritable handle, and TcpStream::try_clone makes \
             inheritable sockets, which a killed child resets. Ruled out: std offers the attribute \
             only on nightly (raw_attribute); portable-pty starts its child without inheriting \
             handles but cannot start a pipe-only process; no maintained crate wraps a handle list \
             over pipes",
        miri: "not reachable: every unsafe call is a Win32 call (CreateProcessW, handle and attribute \
           list functions), which Miri does not run; covered by tests/exec.rs and \
           platform/inherit_tests.rs on Windows",
    },
];

/// Whether `line` holds the keyword `unsafe` outside a comment and a one-line string.
fn holds_unsafe_keyword(line: &str) -> bool {
    let code = line.split("//").next().unwrap_or("");
    code.match_indices("unsafe").any(|(at, _)| {
        let before = code[..at].chars().next_back();
        let after = code[at + "unsafe".len()..].chars().next();
        !before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '"')
            && !after.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '"')
    })
}

/// Whether `source` holds an `unsafe` block, function, impl or trait outside comments and strings of
/// one line; `unsafe_code` (the lint) is not one.
fn holds_unsafe(source: &str) -> bool {
    source.lines().any(holds_unsafe_keyword)
}

/// The lines (1-based) of `unsafe` that no `// SAFETY:` comment or `# Safety` section precedes: the
/// comment lines directly above the line, or above the statement it is in, are read.
fn without_safety_comment(source: &str) -> Vec<usize> {
    let lines: Vec<&str> = source.lines().collect();
    let mut missing = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if !holds_unsafe_keyword(line) {
            continue;
        }
        // A comment may sit above the statement an `unsafe` block is the end of.
        let mut at = index;
        let mut documented = false;
        loop {
            let mut above = at;
            while above > 0 {
                let text = lines[above - 1].trim_start();
                if text.starts_with("//") || text.starts_with("#[") {
                    documented |= text.contains("SAFETY:") || text.contains("# Safety");
                    above -= 1;
                } else {
                    break;
                }
            }
            if documented {
                break;
            }
            // A line that continues a statement (it does not start one) is searched upward once more.
            let previous = if above > 0 {
                lines[above - 1].trim_end()
            } else {
                ""
            };
            if above > 0
                && !(previous.ends_with(';')
                    || previous.ends_with('{')
                    || previous.ends_with('}')
                    || previous.is_empty())
            {
                at = above - 1;
                continue;
            }
            break;
        }
        if !documented {
            missing.push(index + 1);
        }
    }
    missing
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

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn relative(root: &Path, file: &Path) -> String {
    file.strip_prefix(root)
        .expect("under the workspace")
        .to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_owned()
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
fn the_safety_check_wants_a_comment_above_each_unsafe() {
    let documented = "fn f() {\n    // SAFETY: a valid pointer.\n    let a = unsafe { g() };\n}\n";
    assert!(without_safety_comment(documented).is_empty());
    let above_statement =
        "fn f() {\n    // SAFETY: valid.\n    let ok = foo(\n        unsafe { g() },\n    );\n}\n";
    assert!(without_safety_comment(above_statement).is_empty());
    let section =
        "/// Does it.\n///\n/// # Safety\n///\n/// A valid pointer.\npub unsafe fn f() {}\n";
    assert!(without_safety_comment(section).is_empty());
    let bare = "fn f() {\n    let a = unsafe { g() };\n}\n";
    assert_eq!(without_safety_comment(bare), vec![2]);
    let stale = "fn f() {\n    // SAFETY: of the first.\n    let a = unsafe { g() };\n    let b = unsafe { h() };\n}\n";
    assert_eq!(without_safety_comment(stale), vec![4]);
}

#[test]
fn only_the_listed_files_hold_unsafe_code() {
    let root = workspace();
    let mut files = Vec::new();
    rust_files(&root.join("crates"), &mut files);
    assert!(!files.is_empty(), "no source file was found");
    let this = Path::new(file!()).file_name().expect("a file name");
    let mut holding = Vec::new();
    let mut allowing = Vec::new();
    for file in files {
        if file.file_name() == Some(this) {
            continue;
        }
        let source = fs::read_to_string(&file).expect("a source file");
        if holds_unsafe(&source) {
            holding.push(relative(&root, &file));
        }
        if source.contains("allow(unsafe_code)") {
            allowing.push(relative(&root, &file));
        }
    }
    holding.sort();
    allowing.sort();
    // Unix-only and Windows-only files are all scanned: the list names a file whatever the platform.
    let listed: Vec<String> = ALLOWED.iter().map(|entry| entry.file.to_owned()).collect();
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
    let lifted: Vec<_> = allowing
        .iter()
        .filter(|file| !listed.contains(file))
        .collect();
    assert!(
        lifted.is_empty(),
        "these files lift the unsafe_code lint and are not in ALLOWED: {lifted:?}"
    );
    let unlifted: Vec<_> = listed
        .iter()
        .filter(|file| !allowing.contains(file))
        .collect();
    assert!(
        unlifted.is_empty(),
        "these listed files do not lift the unsafe_code lint, so the crate's deny stops them: {unlifted:?}"
    );
}

#[test]
fn every_listed_file_says_why_and_whether_miri_reaches_it() {
    for entry in ALLOWED {
        assert!(
            entry.reason.len() > 40 && entry.reason.contains("Ruled out"),
            "{}: the reason says what needed unsafe and what was ruled out",
            entry.file
        );
        assert!(
            entry.miri.len() > 20,
            "{}: the entry says whether Miri reaches the file, and why not when it does not",
            entry.file
        );
    }
}

#[test]
fn every_unsafe_in_a_listed_file_has_a_safety_comment() {
    let root = workspace();
    for entry in ALLOWED {
        let source = fs::read_to_string(root.join(entry.file)).expect("a listed file");
        let missing = without_safety_comment(&source);
        assert!(
            missing.is_empty(),
            "{}: unsafe at lines {missing:?} has no `// SAFETY:` comment above it",
            entry.file
        );
    }
}

#[test]
fn every_module_but_the_listed_ones_forbids_unsafe() {
    // The crate denies unsafe_code; each of these top-level modules (and the files of `platform`
    // that are not listed) forbids it, so a later `allow` in them cannot lift it.
    let root = workspace().join("crates/bohio/src");
    let mut missing = Vec::new();
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    for file in files {
        let name = relative(&root, &file);
        let listed = ALLOWED
            .iter()
            .any(|entry| entry.file == format!("crates/bohio/src/{name}"));
        // Only a module that is not inside another forbidding one needs the line: the top-level
        // files and directories' `mod.rs`, and the files of `platform`.
        let depth = name.matches('/').count();
        let top = depth == 0
            || (depth == 1 && name.ends_with("/mod.rs"))
            || (name.starts_with("platform/") && depth == 1);
        if !top || listed || name == "lib.rs" || name == "platform/mod.rs" {
            continue;
        }
        let source = fs::read_to_string(&file).expect("a source file");
        if !source.contains("#![forbid(unsafe_code)]") {
            missing.push(name);
        }
    }
    assert!(
        missing.is_empty(),
        "these modules hold no unsafe and do not forbid it: {missing:?}"
    );
}
