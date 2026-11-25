// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The suite of the files kind.

use guatiao::library::{Object, ProviderError};

use super::{ContractSettings, describe_of};
use crate::files::{Directory, FilesSource, failure_kind, word};
use crate::model::{FileStat, OpenModel};
use crate::vocab::{self, file_kind};

/// The most listing reads the suite makes of one directory before it calls the listing endless.
const LISTING_READS: usize = 100_000;

fn fails_with(what: &str, result: Result<(), ProviderError>, words: &[&str]) {
    match result {
        Ok(()) => panic!("files: {what} succeeded and must fail with {words:?}"),
        Err(error) => {
            let kind = failure_kind(&error);
            assert!(
                kind.is_some_and(|kind| words.contains(&kind)),
                "files: {what} failed with {kind:?} ({error:?}), not {words:?}"
            );
        }
    }
}

fn names(directory: &dyn Directory, relative: &str) -> Vec<String> {
    let reader = directory
        .read_dir(relative)
        .unwrap_or_else(|error| panic!("files: read_dir `{relative}` failed: {error:?}"));
    let mut found = Vec::new();
    for _ in 0..LISTING_READS {
        let page = reader
            .next(16)
            .unwrap_or_else(|error| panic!("files: a listing read failed: {error:?}"));
        if page.is_empty() {
            found.sort();
            return found;
        }
        assert!(
            page.len() <= 16,
            "files: a listing read returned {} entries for a maximum of 16",
            page.len()
        );
        found.extend(page.into_iter().map(|entry| entry.name));
    }
    panic!("files: a listing of `{relative}` does not end");
}

fn write_file(directory: &dyn Directory, relative: &str, bytes: &[u8]) {
    let file = directory
        .open_file(
            relative,
            OpenModel {
                write: true,
                create_new: true,
                ..OpenModel::default()
            },
        )
        .unwrap_or_else(|error| panic!("files: creating `{relative}` failed: {error:?}"));
    let mut at = 0_u64;
    while at < bytes.len() as u64 {
        let wrote = file
            .write_at(at, &bytes[usize::try_from(at).unwrap_or(usize::MAX)..])
            .unwrap_or_else(|error| panic!("files: write_at failed: {error:?}"));
        assert!(wrote > 0, "files: write_at wrote no byte");
        at += wrote;
    }
}

fn read_file(directory: &dyn Directory, relative: &str) -> Vec<u8> {
    let file = directory
        .open_file(
            relative,
            OpenModel {
                read: true,
                ..OpenModel::default()
            },
        )
        .unwrap_or_else(|error| panic!("files: opening `{relative}` failed: {error:?}"));
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 7];
    loop {
        let count = file
            .read_at(bytes.len() as u64, &mut buffer)
            .unwrap_or_else(|error| panic!("files: read_at failed: {error:?}"));
        if count == 0 {
            return bytes;
        }
        bytes.extend_from_slice(&buffer[..usize::try_from(count).unwrap_or(0)]);
    }
}

fn kind_of(stat: &FileStat) -> &str {
    &stat.kind
}

/// The suite of the files kind. `root` is a directory the caller made for the suite, empty, which the suite works
/// inside and leaves empty: names in it are the source's own, joined with `/`. A source that is not available is
/// checked for saying so and for refusing to open the root, nothing more.
///
/// It checks `describe` against what the source does (a source that lacks a feature answers `unsupported`, one that
/// is `confined` refuses a name that leaves the directory through a link), the failure words, that a listing ends
/// and honours its maximum, and that a file written is the file read.
///
/// # Panics
///
/// With a message naming what the source got wrong.
pub fn files(source: &dyn FilesSource, root: &str, settings: &ContractSettings) {
    let _ = settings;
    let info = describe_of("files", source.describe(), source.describe());
    if !info.available {
        assert!(
            source.open_dir(root).is_err(),
            "files: an unavailable source opened a directory"
        );
        return;
    }
    let directory: Object<dyn Directory> = source
        .open_dir(root)
        .unwrap_or_else(|error| panic!("files: opening the root failed: {error:?}"));
    let directory: &dyn Directory = &*directory;
    let offers = |word: &str| info.offers.iter().any(|offered| offered == word);

    // The root, a regular file, a directory, a listing.
    let stat = directory
        .stat(".", true)
        .unwrap_or_else(|error| panic!("files: stat of the root failed: {error:?}"));
    assert_eq!(
        kind_of(&stat),
        file_kind::DIR,
        "files: the root is a directory"
    );
    assert!(
        names(directory, ".").is_empty(),
        "files: the root is not empty"
    );

    write_file(directory, "a.txt", b"hello, files");
    assert_eq!(read_file(directory, "a.txt"), b"hello, files");
    let stat = directory
        .stat("a.txt", false)
        .expect("files: stat of a file");
    assert_eq!(kind_of(&stat), file_kind::FILE, "files: a file is a file");
    assert_eq!(stat.size, 12, "files: the size of a written file");
    assert!(
        stat.modified.is_some(),
        "files: a file has a modification time"
    );
    fails_with(
        "creating a file that exists",
        directory
            .open_file(
                "a.txt",
                OpenModel {
                    write: true,
                    create_new: true,
                    ..OpenModel::default()
                },
            )
            .map(drop),
        &[word::EXISTS],
    );

    // Positional writes, lengths and the flush.
    let file = directory
        .open_file(
            "a.txt",
            OpenModel {
                read: true,
                write: true,
                ..OpenModel::default()
            },
        )
        .expect("files: reopening for writing");
    file.write_at(7, b"FILES")
        .expect("files: write at an offset");
    file.set_len(9).expect("files: set_len cuts");
    assert_eq!(file.stat().expect("files: stat of an open file").size, 9);
    if offers(vocab::files::SYNC) {
        file.sync().expect("files: sync");
    } else {
        fails_with("sync", file.sync(), &[word::UNSUPPORTED]);
    }
    drop(file);
    assert_eq!(read_file(directory, "a.txt"), b"hello, FI");
    assert_eq!(directory.stat("a.txt", false).expect("stat").size, 9);

    directory
        .create_dir("d", None, false)
        .expect("files: create_dir");
    fails_with(
        "creating a directory that exists",
        directory.create_dir("d", None, false),
        &[word::EXISTS],
    );
    directory
        .create_dir("p/q/r", None, true)
        .expect("files: create_dir with parents");
    directory
        .create_dir("p/q/r", None, true)
        .expect("files: create_dir with parents over what exists");
    assert_eq!(names(directory, "."), ["a.txt", "d", "p"]);
    assert_eq!(names(directory, "p/q"), ["r"]);
    let nested = directory
        .open_dir("p/q")
        .expect("files: open_dir below the root");
    write_file(&*nested, "inside.txt", b"x");
    assert_eq!(read_file(directory, "p/q/inside.txt"), b"x");

    // Renames.
    directory
        .rename("a.txt", "b.txt", false)
        .expect("files: rename");
    fails_with(
        "a rename of a name that is gone",
        directory.rename("a.txt", "c.txt", false),
        &[word::NOT_FOUND],
    );
    write_file(directory, "c.txt", b"other");
    fails_with(
        "a rename over a name without overwrite",
        directory.rename("b.txt", "c.txt", false),
        &[word::EXISTS],
    );
    if offers(vocab::files::ATOMIC_REPLACE) {
        directory
            .rename("b.txt", "c.txt", true)
            .expect("files: rename with overwrite");
        assert_eq!(read_file(directory, "c.txt"), b"hello, FI");
    } else {
        fails_with(
            "an overwriting rename",
            directory.rename("b.txt", "c.txt", true),
            &[word::UNSUPPORTED],
        );
        directory.remove_file("b.txt").expect("files: remove_file");
    }

    // Links.
    if offers(vocab::files::HARD_LINK) {
        directory
            .hard_link("c.txt", "h.txt")
            .expect("files: hard_link");
        assert_eq!(read_file(directory, "h.txt"), b"hello, FI");
        directory
            .remove_file("h.txt")
            .expect("files: remove a hard link");
    } else {
        fails_with(
            "a hard link",
            directory.hard_link("c.txt", "h.txt"),
            &[word::UNSUPPORTED],
        );
    }
    let linked = offers(vocab::files::SYMLINK) && directory.symlink("c.txt", "l", false).is_ok();
    if linked {
        assert_eq!(directory.read_link("l").expect("files: read_link"), "c.txt");
        let stat = directory.stat("l", false).expect("files: stat of a link");
        assert_eq!(
            kind_of(&stat),
            file_kind::SYMLINK,
            "files: a link is a link"
        );
        assert_eq!(read_file(directory, "l"), b"hello, FI");
        directory.remove_file("l").expect("files: remove a link");
        if offers(vocab::files::CONFINED) {
            directory
                .symlink("..", "up", true)
                .expect("files: a link that leaves the directory");
            for relative in ["up/escape.txt", "up/up"] {
                let result = directory
                    .open_file(
                        relative,
                        OpenModel {
                            read: true,
                            ..OpenModel::default()
                        },
                    )
                    .map(drop);
                fails_with(
                    "a confined name through a link that leaves the directory",
                    result,
                    &[word::OUTSIDE, word::DENIED, word::NOT_FOUND],
                );
            }
            fails_with(
                "a confined stat through a link that leaves the directory",
                directory.stat("up/..", true).map(drop),
                &[word::OUTSIDE, word::DENIED, word::NOT_FOUND],
            );
            directory.remove_file("up").expect("files: remove the link");
        }
    }

    // Attributes where offered.
    if offers(vocab::files::SET_MODE) {
        directory.set_mode("c.txt", 0o640).expect("files: set_mode");
        assert_eq!(
            directory
                .stat("c.txt", false)
                .expect("stat")
                .mode
                .map(|mode| mode & 0o777),
            Some(0o640)
        );
    } else {
        fails_with(
            "set_mode",
            directory.set_mode("c.txt", 0o640),
            &[word::UNSUPPORTED],
        );
    }
    if offers(vocab::files::SET_TIMES) {
        let moved = crate::model::TimeModel {
            secs: 1_000_000_000,
            nanos: 0,
        };
        directory
            .set_times("c.txt", None, Some(moved))
            .expect("files: set_times");
        assert_eq!(
            directory
                .stat("c.txt", false)
                .expect("stat")
                .modified
                .map(|time| time.secs),
            Some(1_000_000_000)
        );
    }
    if offers(vocab::files::STATVFS) {
        let space = directory.statvfs(".").expect("files: statvfs");
        assert!(space.block_size > 0, "files: statvfs gives no block size");
    } else {
        assert!(
            directory.statvfs(".").is_err(),
            "files: statvfs answered though the source lacks it"
        );
    }

    // What a wrong but possible argument does: a failure, never a panic or a hang.
    for relative in ["no-such", "no-such/deeper", "c.txt/under-a-file"] {
        assert!(
            directory.stat(relative, true).is_err(),
            "files: stat of `{relative}` succeeded"
        );
    }
    fails_with(
        "read_dir of a file",
        directory.read_dir("c.txt").map(drop),
        &[
            word::NOT_A_DIRECTORY,
            word::INVALID,
            word::IO,
            word::NOT_FOUND,
        ],
    );
    fails_with(
        "remove_dir of a directory that holds something",
        directory.remove_dir("p"),
        &[word::NOT_EMPTY, word::IO, word::INVALID],
    );

    // Cleaning up.
    directory
        .remove_dir_all("p")
        .expect("files: remove_dir_all");
    directory.remove_dir("d").expect("files: remove_dir");
    directory.remove_file("c.txt").expect("files: remove_file");
    assert!(
        names(directory, ".").is_empty(),
        "files: the root is not empty after the suite cleaned it"
    );
}
