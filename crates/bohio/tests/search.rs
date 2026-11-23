//! `bohio-search/1`, served and driven in-process over memory channels.

#![forbid(unsafe_code)]

mod support;

use std::path::{Path, PathBuf};

use bohio::config::{FilesystemConfig, SearchConfig};
use bohio::{Grant, Permission};
use bohio_client::search::{SearchOptions, SearchResults, search};
use bohio_client::{Error, Open};
use bohio_proto::search::{SearchOutput, SearchRequest, SearchScope};
use bohio_proto::{ErrorCode, protocol};
use support::{Link, WAIT};
use tempfile::TempDir;

const READ: [Permission; 1] = [Permission::FsRead];

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    outside: PathBuf,
    link: Link,
}

fn fixture_with(search: SearchConfig) -> Fixture {
    let tmp = tempfile::tempdir().expect("a scratch directory");
    let root = tmp.path().join("root");
    let outside = tmp.path().join("outside");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&outside).unwrap();
    // `exec` is not granted: a read-only peer searches.
    let grant = Grant::allowing(READ).with_fs_roots([root.clone()]);
    let link = Link::files(grant, FilesystemConfig::default()).searching(search);
    Fixture {
        _tmp: tmp,
        root,
        outside,
        link,
    }
}

fn fixture() -> Fixture {
    fixture_with(SearchConfig::default())
}

fn put(root: &Path, path: &str, content: impl AsRef<[u8]>) {
    let target = root.join(path);
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(target, content).unwrap();
}

fn run(link: &Link, roots: &[&str], pattern: &str, options: &SearchOptions) -> SearchResults {
    search(link, roots, pattern, options)
        .unwrap()
        .finish()
        .unwrap()
}

fn refused(link: &Link, roots: &[&str], pattern: &str, options: &SearchOptions) -> ErrorCode {
    match search(link, roots, pattern, options).unwrap().finish() {
        Err(Error::Remote(body)) => body.code,
        other => panic!("expected the host to refuse, got {other:?}"),
    }
}

/// `path:line` of each match.
fn places(results: &SearchResults) -> Vec<String> {
    results
        .matches
        .iter()
        .map(|found| format!("{}:{}", found.path, found.hit.line))
        .collect()
}

#[test]
fn a_peer_with_fs_read_and_no_exec_finds_lines_with_their_numbers_and_context() {
    let f = fixture();
    put(
        &f.root,
        "src/a.txt",
        "one\ntwo needle\nthree\nfour\nfive needle\nsix\n",
    );
    put(&f.root, "b.txt", "nothing here\r\nneedle at the end");
    let options = SearchOptions {
        context: 1,
        ..SearchOptions::default()
    };
    let results = run(&f.link, &["."], "needle", &options);
    assert_eq!(places(&results), ["b.txt:2", "src/a.txt:2", "src/a.txt:5"]);
    let first = &results.matches[1];
    assert_eq!(first.hit.text, "two needle");
    assert_eq!(first.hit.before, ["one"]);
    assert_eq!(first.hit.after, ["three"]);
    assert_eq!(first.size, 42);
    // The last line of a file has no line after it, and a CRLF is not part of the line before.
    let end = &results.matches[0];
    assert_eq!(end.hit.before, ["nothing here"]);
    assert!(end.hit.after.is_empty());
    let last = &results.matches[2];
    assert_eq!(last.hit.before, ["four"]);
    assert_eq!(last.hit.after, ["six"]);
    assert_eq!(results.done.files, 2);
    assert_eq!(results.done.matches, 3);
    assert!(!results.done.truncated);
    assert!(results.failures.is_empty());
}

#[test]
fn overlapping_context_is_sent_with_each_match() {
    let f = fixture();
    put(&f.root, "a.txt", "a\nhit 1\nhit 2\nb\n");
    let options = SearchOptions {
        context: 2,
        ..SearchOptions::default()
    };
    let results = run(&f.link, &["a.txt"], "hit", &options);
    assert_eq!(places(&results), [":2", ":3"]);
    assert_eq!(results.matches[0].hit.before, ["a"]);
    assert_eq!(results.matches[0].hit.after, ["hit 2", "b"]);
    assert_eq!(results.matches[1].hit.before, ["a", "hit 1"]);
    assert_eq!(results.matches[1].hit.after, ["b"]);
}

#[test]
fn literal_text_and_case_are_the_request_s_choice() {
    let f = fixture();
    put(&f.root, "a.txt", "a.c\nabc\nA.C\n");
    let plain = run(&f.link, &["."], "a.c", &SearchOptions::default());
    assert_eq!(places(&plain), ["a.txt:1", "a.txt:2"]);
    let literal = run(
        &f.link,
        &["."],
        "a.c",
        &SearchOptions {
            literal: true,
            ..SearchOptions::default()
        },
    );
    assert_eq!(places(&literal), ["a.txt:1"]);
    let folded = run(
        &f.link,
        &["."],
        "a.c",
        &SearchOptions {
            literal: true,
            case_insensitive: true,
            ..SearchOptions::default()
        },
    );
    assert_eq!(places(&folded), ["a.txt:1", "a.txt:3"]);
}

#[test]
fn include_exclude_depth_and_hidden_choose_the_files() {
    let f = fixture();
    put(&f.root, "a.rs", "needle");
    put(&f.root, "a.txt", "needle");
    put(&f.root, "sub/b.rs", "needle");
    put(&f.root, "sub/deep/c.rs", "needle");
    put(&f.root, "target/d.rs", "needle");
    put(&f.root, ".hidden/e.rs", "needle");
    put(&f.root, ".dot.rs", "needle");
    let find = |options: SearchOptions| places(&run(&f.link, &["."], "needle", &options));
    assert_eq!(
        find(SearchOptions::default()),
        [
            "a.rs:1",
            "a.txt:1",
            "sub/b.rs:1",
            "sub/deep/c.rs:1",
            "target/d.rs:1"
        ]
    );
    assert_eq!(
        find(SearchOptions {
            include: vec!["*.rs".into()],
            exclude: vec!["target".into(), "deep".into()],
            ..SearchOptions::default()
        }),
        ["a.rs:1", "sub/b.rs:1"]
    );
    assert_eq!(
        find(SearchOptions {
            depth: Some(1),
            ..SearchOptions::default()
        }),
        ["a.rs:1", "a.txt:1"]
    );
    assert_eq!(
        find(SearchOptions {
            depth: Some(2),
            ..SearchOptions::default()
        }),
        ["a.rs:1", "a.txt:1", "sub/b.rs:1", "target/d.rs:1"]
    );
    assert_eq!(
        find(SearchOptions {
            hidden: true,
            ..SearchOptions::default()
        }),
        [
            ".dot.rs:1",
            ".hidden/e.rs:1",
            "a.rs:1",
            "a.txt:1",
            "sub/b.rs:1",
            "sub/deep/c.rs:1",
            "target/d.rs:1"
        ]
    );
}

#[test]
fn binary_large_and_long_lines_are_skipped_counted_or_cut() {
    let f = fixture_with(SearchConfig {
        max_file_bytes: 1000,
        max_line_bytes: 20,
        binary_probe: 16,
        ..SearchConfig::default()
    });
    put(
        &f.root,
        "text.txt",
        format!("needle {}\nshort needle\n", "x".repeat(200)),
    );
    let mut binary = b"needle\n".to_vec();
    binary.push(0);
    binary.extend_from_slice(b"more");
    put(&f.root, "bin.dat", binary);
    put(&f.root, "big.txt", format!("needle\n{}", "y".repeat(2000)));
    // A NUL past the probe does not make a file binary.
    let mut late = b"needle again ".repeat(3);
    late.push(0);
    put(&f.root, "late.dat", late);
    // Only the lines are cut: a multi-byte character is never split.
    put(&f.root, "wide.txt", format!("needle {}", "é".repeat(30)));
    let results = run(&f.link, &["."], "needle", &SearchOptions::default());
    assert_eq!(
        places(&results),
        ["late.dat:1", "text.txt:1", "text.txt:2", "wide.txt:1"]
    );
    assert_eq!(results.done.skipped.binary, 1, "{:?}", results.done);
    assert_eq!(results.done.skipped.large, 1);
    assert_eq!(results.done.skipped.unreadable, 0);
    assert_eq!(results.done.files, 4);
    let cut = &results.matches[1];
    assert!(cut.hit.truncated);
    assert_eq!(cut.hit.text.len(), 20);
    assert!(!results.matches[2].hit.truncated);
    let wide = &results.matches[3];
    assert!(wide.hit.truncated);
    assert!(wide.hit.text.len() <= 20 && wide.hit.text.starts_with("needle é"));
}

#[test]
fn a_bound_on_matches_or_files_ends_the_search_marked_truncated() {
    let f = fixture_with(SearchConfig {
        max_matches: 5,
        ..SearchConfig::default()
    });
    put(&f.root, "a.txt", "hit\n".repeat(10));
    put(&f.root, "b.txt", "hit\n".repeat(10));
    let results = run(&f.link, &["."], "hit", &SearchOptions::default());
    assert_eq!(results.matches.len(), 5);
    assert!(results.done.truncated);
    assert_eq!(results.done.matches, 5);
    // A request may ask for fewer, never for more.
    let fewer = run(
        &f.link,
        &["."],
        "hit",
        &SearchOptions {
            max_matches: Some(2),
            ..SearchOptions::default()
        },
    );
    assert_eq!(fewer.matches.len(), 2);
    let more = run(
        &f.link,
        &["."],
        "hit",
        &SearchOptions {
            max_matches: Some(500),
            ..SearchOptions::default()
        },
    );
    assert_eq!(more.matches.len(), 5);
    let g = fixture();
    for name in ["a", "b", "c"] {
        put(&g.root, &format!("{name}.txt"), "hit\n");
    }
    let two_files = run(
        &g.link,
        &["."],
        "hit",
        &SearchOptions {
            max_files: Some(2),
            ..SearchOptions::default()
        },
    );
    assert_eq!(two_files.done.files, 2);
    assert!(two_files.done.truncated);
    assert_eq!(places(&two_files), ["a.txt:1", "b.txt:1"]);
    // Pending context is sent when a bound ends the search.
    let context = run(
        &f.link,
        &["a.txt"],
        "hit",
        &SearchOptions {
            context: 1,
            max_matches: Some(1),
            ..SearchOptions::default()
        },
    );
    assert_eq!(context.matches.len(), 1);
    assert_eq!(context.matches[0].hit.after, ["hit"]);
}

#[test]
fn a_request_that_cannot_be_run_is_bad_request_and_the_peer_s_pattern_is_not_echoed() {
    let f = fixture_with(SearchConfig {
        regex_size_limit: 10_000,
        max_context: 3,
        ..SearchConfig::default()
    });
    put(&f.root, "a.txt", "hello");
    let plain = SearchOptions::default();
    assert_eq!(refused(&f.link, &["."], "", &plain), ErrorCode::BadRequest);
    assert_eq!(refused(&f.link, &["."], "(", &plain), ErrorCode::BadRequest);
    // A program larger than the host allows compiles nowhere.
    assert_eq!(
        refused(&f.link, &["."], "(\\w{100}){100}", &plain),
        ErrorCode::BadRequest
    );
    assert_eq!(refused(&f.link, &[], "x", &plain), ErrorCode::BadRequest);
    assert_eq!(
        refused(
            &f.link,
            &["."],
            "x",
            &SearchOptions {
                context: 4,
                ..SearchOptions::default()
            }
        ),
        ErrorCode::BadRequest
    );
    assert_eq!(
        refused(
            &f.link,
            &["."],
            "x",
            &SearchOptions {
                max_matches: Some(0),
                ..SearchOptions::default()
            }
        ),
        ErrorCode::BadRequest
    );
    assert_eq!(
        refused(
            &f.link,
            &["."],
            "x",
            &SearchOptions {
                include: vec!["[".into()],
                ..SearchOptions::default()
            }
        ),
        ErrorCode::BadRequest
    );
    // The same pattern as plain text is fine.
    let literal = SearchOptions {
        literal: true,
        ..SearchOptions::default()
    };
    assert!(run(&f.link, &["."], "(", &literal).matches.is_empty());
    // The refusal does not repeat the pattern.
    let secret = "(secret-value";
    let Err(Error::Remote(body)) = search(&f.link, &["."], secret, &plain).unwrap().finish() else {
        panic!("a pattern that does not compile is refused");
    };
    assert!(!body.message.contains("secret-value"), "{}", body.message);
}

#[test]
fn a_pattern_that_would_backtrack_forever_elsewhere_finishes() {
    let f = fixture();
    put(&f.root, "a.txt", format!("{}!\n", "a".repeat(5000)));
    let results = run(&f.link, &["."], "(a+)+$", &SearchOptions::default());
    assert!(results.matches.is_empty());
    assert_eq!(results.done.files, 1);
}

#[test]
fn a_path_outside_the_roots_is_an_item_error_and_nothing_outside_is_read() {
    let f = fixture();
    put(&f.root, "in.txt", "needle");
    put(&f.outside, "out.txt", "needle");
    let outside = f.outside.to_string_lossy().into_owned();
    let results = run(
        &f.link,
        &[outside.as_str(), "../outside", "."],
        "needle",
        &SearchOptions::default(),
    );
    assert_eq!(results.matches.len(), 1);
    assert_eq!(results.matches[0].root, 2);
    assert_eq!(results.failures.len(), 2);
    assert!(
        results
            .failures
            .iter()
            .all(|failure| failure.code == ErrorCode::OutsideRoot),
        "{:?}",
        results.failures
    );
    assert_eq!(
        results.failures.iter().map(|f| f.root).collect::<Vec<_>>(),
        [0, 1]
    );
}

#[test]
fn a_link_is_never_followed_whether_it_leads_out_of_the_root_or_stays_inside() {
    let f = fixture();
    put(&f.root, "real/in.txt", "needle");
    put(&f.outside, "out.txt", "needle");
    put(&f.outside, "sub/more.txt", "needle");
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink("../outside", f.root.join("escape"))
        .and_then(|()| std::os::unix::fs::symlink("real", f.root.join("shortcut")))
        .and_then(|()| std::os::unix::fs::symlink("../outside/out.txt", f.root.join("file")));
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_dir("..\\outside", f.root.join("escape"))
        .and_then(|()| std::os::windows::fs::symlink_dir("real", f.root.join("shortcut")))
        .and_then(|()| {
            std::os::windows::fs::symlink_file("..\\outside\\out.txt", f.root.join("file"))
        });
    if made.is_err() {
        println!("SKIPPED: this process may not make a symbolic link");
        return;
    }
    let results = run(&f.link, &["."], "needle", &SearchOptions::default());
    assert_eq!(places(&results), ["real/in.txt:1"]);
    // A link is left alone, not refused: the search reports nothing about it.
    assert!(results.failures.is_empty(), "{:?}", results.failures);
    // A root that is itself a link is not followed either.
    let root = run(&f.link, &["escape"], "needle", &SearchOptions::default());
    assert!(root.matches.is_empty());
    assert_eq!(root.failures.len(), 1);
    assert_eq!(root.failures[0].code, ErrorCode::Unsupported);
}

#[test]
fn a_file_can_be_the_root_and_a_missing_one_is_an_item_error() {
    let f = fixture();
    put(&f.root, "one.txt", "x\nneedle\n");
    let results = run(
        &f.link,
        &["one.txt", "missing.txt"],
        "needle",
        &SearchOptions::default(),
    );
    assert_eq!(results.matches.len(), 1);
    assert_eq!(results.matches[0].path, "");
    assert_eq!(results.failures.len(), 1);
    assert_eq!(results.failures[0].code, ErrorCode::NotFound);
    assert_eq!(results.failures[0].root, 1);
}

#[test]
fn a_peer_without_fs_read_is_denied_and_a_host_at_its_limit_is_busy() {
    let tmp = tempfile::tempdir().unwrap();
    let grant = Grant::allowing([Permission::Info]).with_fs_roots([tmp.path().to_owned()]);
    let link = Link::files(grant, FilesystemConfig::default());
    assert_eq!(
        refused(&link, &["."], "x", &SearchOptions::default()),
        ErrorCode::Denied
    );
    let f = fixture_with(SearchConfig {
        max_searches: 1,
        ..SearchConfig::default()
    });
    put(&f.root, "a.txt", "hit\n".repeat(100_000));
    // A search nobody reads holds its slot, so a second one is turned away.
    let mut held = search(&f.link, &["."], "hit", &SearchOptions::default()).unwrap();
    assert!(held.next().is_some());
    assert_eq!(
        refused(&f.link, &["."], "hit", &SearchOptions::default()),
        ErrorCode::Busy
    );
    drop(held);
}

#[test]
fn closing_the_stream_stops_the_host_and_the_raw_frames_decode() {
    let f = fixture();
    put(&f.root, "a.txt", "hit\n".repeat(50_000));
    let request = SearchRequest {
        walk: SearchScope {
            roots: vec![".".into()],
            ..SearchScope::default()
        },
        pattern: "hit".into(),
        ..SearchRequest::default()
    };
    let channel = f
        .link
        .open(protocol::SEARCH, &request.to_metadata().unwrap())
        .unwrap();
    let first = channel.receive(WAIT).unwrap().unwrap();
    assert!(matches!(
        SearchOutput::decode(&first).unwrap(),
        SearchOutput::File(_)
    ));
    channel.close();
    drop(channel);
    // The engine ends on its own once its stream is closed: finished_fs waits for it.
    let served = f.link.finished_fs();
    assert_eq!(served.len(), 1);
}
