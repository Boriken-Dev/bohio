//! What a search holds for a file whose one long line is not UTF-8.

#![cfg(target_os = "linux")]
#![forbid(unsafe_code)]

mod support;

use bohio::config::{FilesystemConfig, SearchConfig};
use bohio::{Grant, Permission};
use bohio_client::search::{SearchOptions, search};
use support::Link;

/// The process's peak resident set, in bytes, from `/proc/self/status`.
fn peak() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let line = status
        .lines()
        .find(|line| line.starts_with("VmHWM:"))
        .unwrap();
    let kib: u64 = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    kib * 1024
}

/// Bytes of a line that is not UTF-8 are matched as bytes, not made into text first (which takes three bytes for each):
/// a search of one such line holds the file and little more.
#[test]
fn a_line_of_bytes_that_are_not_utf8_is_not_held_again_as_text() {
    const SIZE: usize = 16 << 20;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("blob"), vec![0x80_u8; SIZE]).unwrap();
    let grant = Grant::allowing([Permission::FsRead]).with_fs_roots([root]);
    let link = Link::files(grant, FilesystemConfig::default()).searching(SearchConfig {
        max_file_bytes: SIZE as u64,
        ..SearchConfig::default()
    });
    let before = peak();
    let found = search(&link, &["."], "needle", &SearchOptions::default())
        .unwrap()
        .finish()
        .unwrap();
    let grown = peak().saturating_sub(before);
    assert!(found.matches.is_empty());
    // The file itself is 16 MiB; the text made of its line would be 48 MiB more, and a buffer that doubles another 16.
    assert!(
        grown < 2 * SIZE as u64,
        "a search of {SIZE} bytes grew the process by {grown}"
    );
}
