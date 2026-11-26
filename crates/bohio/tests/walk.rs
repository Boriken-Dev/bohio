//! `bohio-walk/1` and `RangeReader`, served and driven in-process over memory channels.

#![forbid(unsafe_code)]

mod support;

use std::io::{Read, Seek, SeekFrom};
use std::path::PathBuf;

use bohio::config::{FilesystemConfig, WalkConfig};
use bohio::{Grant, Permission};
use bohio_client::range::{RangeOptions, RangeReader};
use bohio_client::walk::{WalkItem, WalkOptions, walk};
use bohio_client::{Error, Open};
use bohio_proto::compress::{Codec, Compression};
use bohio_proto::fs::FileKind;
use bohio_proto::hash::Algorithm;
use bohio_proto::walk::{WalkInput, WalkOutput, WalkRequest, WalkRoot};
use bohio_proto::{ErrorCode, protocol};
use bohio_transfer::hash::Hasher;
use support::{Link, WAIT};
use tempfile::TempDir;

const READ: [Permission; 1] = [Permission::FsRead];

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    link: Link,
}

fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("a scratch directory");
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(tmp.path().join("secret.txt"), b"beyond the root").unwrap();
    let grant = Grant::allowing(READ).with_fs_roots([root.clone()]);
    let link = Link::files(grant, FilesystemConfig::default());
    Fixture {
        _tmp: tmp,
        root,
        link,
    }
}

fn code<T: std::fmt::Debug>(result: Result<T, Error>) -> ErrorCode {
    match result {
        Err(Error::Remote(body)) => body.code,
        other => panic!("expected the host to refuse, got {other:?}"),
    }
}

fn pseudo(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}

/// The paths of the entries a walk of `roots` listed, in the order they came, with the failures.
fn listed(link: &Link, roots: &[&str], options: &WalkOptions) -> (Vec<(u64, String)>, Vec<String>) {
    let mut entries = Vec::new();
    let mut failures = Vec::new();
    for item in walk(link, roots, options).unwrap() {
        match item.unwrap() {
            WalkItem::Entry { root, entry } => entries.push((root, entry.path)),
            WalkItem::Failure(failure) => failures.push(format!(
                "{}:{:?}",
                failure.path.unwrap_or_default(),
                failure.code
            )),
        }
    }
    (entries, failures)
}

#[test]
fn a_walk_of_ten_thousand_files_is_one_stream_in_depth_first_sorted_order() {
    let f = fixture();
    let mut expected: Vec<String> = Vec::new();
    for dir in 0..100_u32 {
        let name = format!("d{dir:03}");
        std::fs::create_dir(f.root.join(&name)).unwrap();
        expected.push(name.clone());
        for file in 0..100_u32 {
            let leaf = format!("f{file:03}.txt");
            std::fs::write(f.root.join(&name).join(&leaf), b"x").unwrap();
            expected.push(format!("{name}/{leaf}"));
        }
    }
    let mut iter = walk(&f.link, &["."], &WalkOptions::default()).unwrap();
    let mut seen = Vec::new();
    for item in iter.by_ref() {
        if let WalkItem::Entry { root, entry } = item.unwrap() {
            assert_eq!(root, 0);
            seen.push(entry);
        }
    }
    // The root itself first, with an empty path, then each directory followed by its names.
    assert_eq!(seen[0].path, "");
    assert_eq!(seen[0].metadata.kind, FileKind::Directory);
    let paths: Vec<&str> = seen[1..].iter().map(|entry| entry.path.as_str()).collect();
    assert_eq!(
        paths,
        expected.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert_eq!(iter.done().unwrap().entries, 10_101);
    assert!(
        seen.iter()
            .filter(|entry| entry.metadata.kind == FileKind::File)
            .all(|entry| entry.metadata.size == 1)
    );
    drop(iter);
    // One stream served the whole tree.
    let served = f.link.finished_fs();
    assert_eq!(served.len(), 1);
    assert!(served[0].is_ok());
}

#[test]
fn depth_zero_is_a_batched_stat_of_five_hundred_roots_and_depth_one_stays_shallow() {
    let f = fixture();
    let mut names = Vec::new();
    for index in 0..500 {
        let name = format!("r{index:03}");
        std::fs::write(f.root.join(&name), name.as_bytes()).unwrap();
        names.push(name);
    }
    std::fs::create_dir_all(f.root.join("deep/er/still")).unwrap();
    std::fs::write(f.root.join("deep/er/still/x"), b"x").unwrap();
    let roots: Vec<&str> = names.iter().map(String::as_str).collect();
    let options = WalkOptions {
        depth: Some(0),
        ..WalkOptions::default()
    };
    let (entries, failures) = listed(&f.link, &roots, &options);
    assert!(failures.is_empty());
    assert_eq!(entries.len(), 500);
    for (position, (root, path)) in entries.iter().enumerate() {
        assert_eq!((*root, path.as_str()), (position as u64, ""));
    }
    let one = WalkOptions {
        depth: Some(1),
        ..WalkOptions::default()
    };
    let (entries, _) = listed(&f.link, &["deep"], &one);
    let paths: Vec<&str> = entries.iter().map(|(_, path)| path.as_str()).collect();
    assert_eq!(paths, ["", "er"]);
    // A root that is missing is an item error, and the roots after it are still walked.
    let (entries, failures) = listed(&f.link, &["r000", "missing", "r001"], &options);
    assert_eq!(entries.len(), 2);
    assert_eq!(failures, [":NotFound"]);
}

#[test]
fn include_and_exclude_globs_pick_entries_and_exclude_prunes_a_subtree() {
    let f = fixture();
    for dir in ["src", "src/inner", "target", "target/debug"] {
        std::fs::create_dir_all(f.root.join(dir)).unwrap();
    }
    for file in [
        "src/a.rs",
        "src/b.txt",
        "src/inner/c.rs",
        "target/debug/d.rs",
        "e.rs",
    ] {
        std::fs::write(f.root.join(file), b"x").unwrap();
    }
    let rust = WalkOptions {
        include: vec!["*.rs".into()],
        exclude: vec!["target".into()],
        ..WalkOptions::default()
    };
    let (entries, failures) = listed(&f.link, &["."], &rust);
    assert!(failures.is_empty());
    let paths: Vec<&str> = entries.iter().map(|(_, path)| path.as_str()).collect();
    // The root is always listed; directories are entered though they do not match; `target` is pruned.
    assert_eq!(paths, ["", "e.rs", "src/a.rs", "src/inner/c.rs"]);
    let by_path = WalkOptions {
        include: vec!["src/*.txt".into()],
        ..WalkOptions::default()
    };
    let (entries, _) = listed(&f.link, &["."], &by_path);
    let paths: Vec<&str> = entries.iter().map(|(_, path)| path.as_str()).collect();
    assert_eq!(paths, ["", "src/b.txt"]);
    // A glob that does not parse refuses the stream.
    let broken = WalkOptions {
        include: vec!["[".into()],
        ..WalkOptions::default()
    };
    assert_eq!(
        code(walk(&f.link, &["."], &broken).map(|_| ())),
        ErrorCode::BadRequest
    );
}

#[test]
fn a_link_leaving_the_root_is_described_and_followed_it_is_an_item_error() {
    let f = fixture();
    std::fs::write(f.root.join("real.txt"), b"real").unwrap();
    std::fs::create_dir(f.root.join("sub")).unwrap();
    let made = {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("../secret.txt", f.root.join("out"))
        }
        #[cfg(windows)]
        {
            std::os::windows::fs::symlink_file("..\\secret.txt", f.root.join("out"))
        }
    };
    if made.is_err() {
        println!("SKIPPED: this process may not make a symbolic link");
        return;
    }
    let (entries, failures) = listed(&f.link, &["."], &WalkOptions::default());
    assert!(failures.is_empty());
    let out: Vec<_> = entries.iter().filter(|(_, path)| path == "out").collect();
    assert_eq!(out.len(), 1, "the link is described, not followed");
    let mut found = None;
    for item in walk(&f.link, &["."], &WalkOptions::default()).unwrap() {
        if let WalkItem::Entry { entry, .. } = item.unwrap()
            && entry.path == "out"
        {
            found = Some(entry);
        }
    }
    let entry = found.unwrap();
    assert_eq!(entry.metadata.kind, FileKind::Symlink);
    assert!(
        entry
            .target
            .as_deref()
            .is_some_and(|target| target.contains("secret.txt"))
    );
    // Followed, a link that leaves the root is an item error and the walk goes on.
    let follow = WalkOptions {
        follow: true,
        ..WalkOptions::default()
    };
    let (entries, failures) = listed(&f.link, &["."], &follow);
    assert!(entries.iter().any(|(_, path)| path == "real.txt"));
    assert_eq!(failures, ["out:OutsideRoot"]);
}

#[test]
fn a_followed_link_to_a_directory_is_entered_once_and_a_loop_is_not_entered_twice() {
    let f = fixture();
    std::fs::create_dir_all(f.root.join("a/b")).unwrap();
    std::fs::write(f.root.join("a/b/leaf"), b"leaf").unwrap();
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink("..", f.root.join("a/b/up"))
        .and_then(|()| std::os::unix::fs::symlink("a/b", f.root.join("shortcut")));
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_dir("..", f.root.join("a\\b\\up"))
        .and_then(|()| std::os::windows::fs::symlink_dir("a\\b", f.root.join("shortcut")));
    if made.is_err() {
        println!("SKIPPED: this process may not make a symbolic link");
        return;
    }
    // Not followed: the links are entries and nothing is entered through them.
    let (entries, _) = listed(&f.link, &["."], &WalkOptions::default());
    let paths: Vec<&str> = entries.iter().map(|(_, path)| path.as_str()).collect();
    assert_eq!(
        paths,
        ["", "a", "a/b", "a/b/leaf", "a/b/up", "shortcut"],
        "links are described"
    );
    let follow = WalkOptions {
        follow: true,
        ..WalkOptions::default()
    };
    let (entries, failures) = listed(&f.link, &["."], &follow);
    let paths: Vec<&str> = entries.iter().map(|(_, path)| path.as_str()).collect();
    if cfg!(unix) {
        // `a/b` is entered as itself first; `up` leads to the root, `shortcut` to `a/b`, both already seen.
        assert_eq!(
            paths,
            ["", "a", "a/b", "a/b/leaf", "a/b/up", "shortcut"],
            "no directory is entered twice"
        );
        assert!(failures.is_empty(), "{failures:?}");
    } else {
        // Without device and inode a link to a directory is not entered: each is an item error.
        assert_eq!(paths.len(), 6);
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert!(
            failures
                .iter()
                .all(|failure| failure.ends_with("Unsupported"))
        );
    }
}

#[test]
fn a_grant_narrowed_while_the_stream_is_open_refuses_the_next_root() {
    let f = fixture();
    std::fs::write(f.root.join("one"), b"1").unwrap();
    std::fs::write(f.root.join("two"), b"2").unwrap();
    let stream = f
        .link
        .open(
            protocol::WALK,
            &WalkRequest::default().to_metadata().unwrap(),
        )
        .unwrap();
    let send = |input: WalkInput| stream.send_timeout(&input.encode().unwrap(), WAIT).unwrap();
    let next = || WalkOutput::decode(&stream.receive(WAIT).unwrap().unwrap()).unwrap();
    assert!(matches!(next(), WalkOutput::Header(_)));
    send(WalkInput::Root(WalkRoot { path: "one".into() }));
    let WalkOutput::Entries(entries) = next() else {
        panic!("the first root is walked");
    };
    assert_eq!(entries.root, 0);
    // From now on the peer may use no filesystem root.
    f.link.narrow(Grant::allowing(READ));
    send(WalkInput::Root(WalkRoot { path: "two".into() }));
    let WalkOutput::ItemError(error) = next() else {
        panic!("the second root is refused");
    };
    assert_eq!(error.root, 1);
    assert_eq!(error.code, ErrorCode::Denied);
    send(WalkInput::End);
    assert!(matches!(next(), WalkOutput::Done(done) if done.entries == 1));
}

#[test]
fn a_compressed_walk_lists_what_a_raw_one_does_in_fewer_bytes() {
    let f = fixture();
    for index in 0..2000 {
        let name = format!("directory-with-a-long-name/file-{index:05}.log");
        std::fs::create_dir_all(f.root.join("directory-with-a-long-name")).unwrap();
        std::fs::write(f.root.join(name), b"x").unwrap();
    }
    let plain = WalkOptions::default();
    let mut raw = walk(&f.link, &["."], &plain).unwrap();
    let raw_paths: Vec<String> = raw
        .by_ref()
        .filter_map(|item| match item.unwrap() {
            WalkItem::Entry { entry, .. } => Some(entry.path),
            WalkItem::Failure(_) => None,
        })
        .collect();
    let raw_wire = raw.wire_bytes();
    for algorithm in [Codec::Zstd, Codec::Lz4, Codec::Deflate] {
        let options = WalkOptions {
            compression: Some(Compression::new(algorithm.clone())),
            ..WalkOptions::default()
        };
        let mut compressed = walk(&f.link, &["."], &options).unwrap();
        assert_eq!(
            compressed.compression().map(|used| used.algorithm.clone()),
            Some(algorithm.clone())
        );
        let paths: Vec<String> = compressed
            .by_ref()
            .filter_map(|item| match item.unwrap() {
                WalkItem::Entry { entry, .. } => Some(entry.path),
                WalkItem::Failure(_) => None,
            })
            .collect();
        assert_eq!(paths, raw_paths, "{algorithm:?}");
        assert!(
            compressed.wire_bytes() * 2 < raw_wire,
            "{algorithm:?}: {} of {raw_wire}",
            compressed.wire_bytes()
        );
    }
}

#[test]
fn the_walks_digests_are_the_files_and_max_entries_ends_it_with_an_item_error() {
    let f = fixture();
    let data = pseudo(70_000, 5);
    std::fs::write(f.root.join("data.bin"), &data).unwrap();
    std::fs::write(f.root.join("small"), b"abc").unwrap();
    let options = WalkOptions {
        hashes: vec![Algorithm::Sha256, Algorithm::Crc32c],
        ..WalkOptions::default()
    };
    for item in walk(&f.link, &["."], &options).unwrap() {
        let WalkItem::Entry { entry, .. } = item.unwrap() else {
            continue;
        };
        if entry.path == "data.bin" {
            let mut hasher = Hasher::new(&[Algorithm::Sha256, Algorithm::Crc32c]).unwrap();
            hasher.update(&data);
            assert_eq!(entry.hashes.unwrap(), hasher.finish());
        } else {
            // A directory has no digests.
            assert!(entry.metadata.kind != FileKind::Directory || entry.hashes.is_none());
        }
    }
    let unknown = WalkOptions {
        hashes: vec![Algorithm::Other("nonsense".into())],
        ..WalkOptions::default()
    };
    assert_eq!(
        code(walk(&f.link, &["."], &unknown).map(|_| ())),
        ErrorCode::Unsupported
    );

    let tight = fixture();
    let link = Link::files(
        Grant::allowing(READ).with_fs_roots([tight.root.clone()]),
        FilesystemConfig::default(),
    )
    .walking(WalkConfig {
        max_entries: 10,
        ..WalkConfig::default()
    });
    for index in 0..30 {
        std::fs::write(tight.root.join(format!("f{index:02}")), b"x").unwrap();
    }
    let mut iter = walk(&link, &[".", "."], &WalkOptions::default()).unwrap();
    let mut entries = 0;
    let mut stops = 0;
    for item in iter.by_ref() {
        match item.unwrap() {
            WalkItem::Entry { .. } => entries += 1,
            WalkItem::Failure(failure) => {
                assert_eq!(failure.code, ErrorCode::Busy);
                stops += 1;
            }
        }
    }
    assert_eq!(entries, 10);
    // The bound is announced once for the root it stopped in and again for the root that came after.
    assert_eq!(stops, 2);
    assert_eq!(iter.done().unwrap().entries, 10);
}

#[test]
fn a_walk_past_max_streams_is_busy_and_the_slot_returns_when_it_ends() {
    let f = fixture();
    let link = Link::files(
        Grant::allowing(READ).with_fs_roots([f.root.clone()]),
        FilesystemConfig::default(),
    )
    .walking(WalkConfig {
        max_streams: 1,
        ..WalkConfig::default()
    });
    // A stream that has been told no roots and no end holds its slot.
    let holding = link
        .open(
            protocol::WALK,
            &WalkRequest::default().to_metadata().unwrap(),
        )
        .unwrap();
    assert!(matches!(
        WalkOutput::decode(&holding.receive(WAIT).unwrap().unwrap()).unwrap(),
        WalkOutput::Header(_)
    ));
    assert_eq!(
        code(walk(&link, &["."], &WalkOptions::default()).map(|_| ())),
        ErrorCode::Busy
    );
    holding.close();
    // The host notices the closed stream and frees the slot.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        match walk(&link, &["."], &WalkOptions::default()) {
            Ok(_) => break,
            Err(Error::Remote(body)) if body.code == ErrorCode::Busy => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the slot never came back"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            Err(other) => panic!("{other:?}"),
        }
    }
}

#[test]
fn a_range_reader_reads_and_seeks_like_a_local_file_under_random_seeks() {
    let f = fixture();
    let data = pseudo(5 * 1024 * 1024 + 123, 11);
    std::fs::write(f.root.join("big.bin"), &data).unwrap();
    for (range, prefetch, compression) in [
        (64 * 1024, 2, None),
        (1 << 20, 0, None),
        (100_000, 3, Some(Compression::new(Codec::Lz4))),
    ] {
        let options = RangeOptions {
            range,
            prefetch,
            compression,
            ..RangeOptions::default()
        };
        let mut reader = RangeReader::open(&f.link, "big.bin", &options).unwrap();
        assert_eq!(reader.len(), data.len() as u64);
        let mut state = 99_u64;
        let mut next = move |bound: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        for _ in 0..60 {
            let at = next(data.len() as u64 + 10);
            let moved = reader.seek(SeekFrom::Start(at)).unwrap();
            assert_eq!(moved, at);
            let want = usize::try_from(next(150_000)).unwrap();
            let mut got = vec![0_u8; want];
            let mut filled = 0;
            while filled < want {
                let count = reader.read(&mut got[filled..]).unwrap();
                if count == 0 {
                    break;
                }
                filled += count;
            }
            let from = usize::try_from(at).unwrap().min(data.len());
            let end = (from + want).min(data.len());
            assert_eq!(&got[..filled], &data[from..end], "range {range} at {at}");
        }
        // Relative seeks agree with the file.
        reader.seek(SeekFrom::End(-10)).unwrap();
        let mut tail = Vec::new();
        reader.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, &data[data.len() - 10..]);
        assert_eq!(
            reader.seek(SeekFrom::Current(-4)).unwrap(),
            data.len() as u64 - 4
        );
        assert!(
            reader
                .seek(SeekFrom::Current(-(data.len() as i64) - 99))
                .is_err()
        );
    }
}

#[test]
fn a_seek_inside_what_was_fetched_costs_no_stream_and_a_far_one_wastes_at_most_the_prefetch() {
    let f = fixture();
    let data = pseudo(1_000_000, 3);
    std::fs::write(f.root.join("big.bin"), &data).unwrap();
    let options = RangeOptions {
        range: 100_000,
        prefetch: 2,
        ..RangeOptions::default()
    };
    let mut reader = RangeReader::open(&f.link, "big.bin", &options).unwrap();
    // Opening asked for the first range and two ahead.
    let mut buffer = vec![0_u8; 50_000];
    reader.read_exact(&mut buffer).unwrap();
    reader.seek(SeekFrom::Start(10)).unwrap();
    reader.read_exact(&mut buffer[..1000]).unwrap();
    assert_eq!(&buffer[..1000], &data[10..1010]);
    reader.seek(SeekFrom::Start(90_000)).unwrap();
    reader.read_exact(&mut buffer[..5000]).unwrap();
    assert_eq!(&buffer[..5000], &data[90_000..95_000]);
    drop(reader);
    // Three streams opened at the start, none for the seeks inside the first range.
    assert_eq!(f.link.finished_fs().len(), 3);
}

#[test]
fn a_root_removed_from_the_peers_roots_while_a_walk_runs_ends_with_outside_root() {
    let f = fixture();
    for index in 0..200 {
        std::fs::write(f.root.join(format!("f{index:03}")), b"x").unwrap();
    }
    let link = Link::files(
        Grant::allowing(READ).with_fs_roots([f.root.clone()]),
        FilesystemConfig::default(),
    )
    .walking(WalkConfig {
        recheck_entries: 10,
        ..WalkConfig::default()
    });
    // The grant is read once for the root, then again every ten entries: the third read finds no root.
    link.narrow_after(2, Grant::allowing(READ));
    let (entries, failures) = listed(&link, &["."], &WalkOptions::default());
    assert!(
        entries.len() < 201,
        "the walk went on: {} entries",
        entries.len()
    );
    // A grant with no root at all is `denied`; one that names other roots is `outside_root`.
    assert_eq!(failures, [":Denied"]);
}
