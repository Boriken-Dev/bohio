//! `move`, `copy` of trees, `truncate`, and the file identity in metadata, served and driven in-process.

#![forbid(unsafe_code)]

mod support;

use std::path::{Path, PathBuf};

use bohio::config::FilesystemConfig;
use bohio::{Grant, Permission};
use bohio_client::Error;
use bohio_client::fs::{CopyOptions, Fs};
use bohio_proto::ErrorCode;
use bohio_proto::fs::FileKind;
use support::Link;
use tempfile::TempDir;

const RW: [Permission; 2] = [Permission::FsRead, Permission::FsWrite];

struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    link: Link,
}

fn fixture(config: FilesystemConfig) -> Fixture {
    let tmp = tempfile::tempdir().expect("a scratch directory");
    let root = tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let grant = Grant::allowing(RW).with_fs_roots([root.clone()]);
    let link = Link::files(grant, config);
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

fn text(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn a_move_within_a_filesystem_renames_and_refuses_an_existing_target_unless_asked() {
    let f = fixture(FilesystemConfig::default());
    let fs = Fs::connect(&f.link).unwrap();
    std::fs::write(f.root.join("a"), "first").unwrap();
    std::fs::write(f.root.join("b"), "second").unwrap();
    assert_eq!(code(fs.move_to("a", "b", false)), ErrorCode::Exists);
    assert_eq!(text(&f.root.join("a")), "first");
    assert_eq!(text(&f.root.join("b")), "second");
    fs.move_to("a", "b", true).unwrap();
    assert!(!f.root.join("a").exists());
    assert_eq!(text(&f.root.join("b")), "first");
    // A directory moves with what is in it, and a free name needs no `overwrite`.
    std::fs::create_dir_all(f.root.join("d/e")).unwrap();
    std::fs::write(f.root.join("d/e/f"), "deep").unwrap();
    fs.move_to("d", "g", false).unwrap();
    assert_eq!(text(&f.root.join("g/e/f")), "deep");
    assert_eq!(code(fs.move_to("missing", "z", false)), ErrorCode::NotFound);
    assert_eq!(
        code(fs.move_to("b", "../out", false)),
        ErrorCode::OutsideRoot
    );
}

#[test]
fn of_two_moves_to_one_free_name_exactly_one_wins_and_nothing_is_replaced() {
    let f = fixture(FilesystemConfig::default());
    for round in 0..200 {
        let (one, two) = (format!("one{round}"), format!("two{round}"));
        let target = format!("target{round}");
        std::fs::write(f.root.join(&one), "one").unwrap();
        std::fs::write(f.root.join(&two), "two").unwrap();
        let results: Vec<Result<(), Error>> = std::thread::scope(|scope| {
            let handles: Vec<_> = [&one, &two]
                .into_iter()
                .map(|from| {
                    let (link, target) = (&f.link, &target);
                    scope.spawn(move || {
                        let fs = Fs::connect(link).unwrap();
                        fs.move_to(from, target, false)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let winners = results.iter().filter(|result| result.is_ok()).count();
        assert_eq!(winners, 1, "round {round}: {results:?}");
        // The loser's source is still there, and the target is the winner's bytes.
        let left = [&one, &two]
            .into_iter()
            .filter(|name| f.root.join(name).exists())
            .count();
        assert_eq!(left, 1, "round {round}");
        let loser = results.iter().position(|result| result.is_err()).unwrap();
        let (kept, won) = if loser == 0 {
            ("one", "two")
        } else {
            ("two", "one")
        };
        assert_eq!(text(&f.root.join(&target)), won, "round {round}");
        assert_eq!(text(&f.root.join(format!("{kept}{round}"))), kept);
    }
    // The same through the old `rename` call.
    let fs = Fs::connect(&f.link).unwrap();
    std::fs::write(f.root.join("x"), "x").unwrap();
    std::fs::write(f.root.join("y"), "y").unwrap();
    assert_eq!(code(fs.rename("x", "y", false)), ErrorCode::Exists);
}

#[test]
fn a_move_across_two_filesystems_copies_with_mode_and_time_and_removes_the_original() {
    // A root under /dev/shm beside one under the temporary directory: two filesystems on Linux.
    let shm = Path::new("/dev/shm");
    if !cfg!(target_os = "linux") || !shm.is_dir() {
        println!("SKIPPED: there is no second filesystem to move across here");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir_in(shm).unwrap();
    let (first, second) = (tmp.path().join("one"), other.path().join("two"));
    std::fs::create_dir(&first).unwrap();
    std::fs::create_dir(&second).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata(&first).unwrap().dev() == std::fs::metadata(&second).unwrap().dev() {
            println!("SKIPPED: /dev/shm shares a filesystem with the temporary directory");
            return;
        }
    }
    let grant = Grant::allowing(RW).with_fs_roots([first.clone(), second.clone()]);
    let link = Link::files(grant, FilesystemConfig::default());
    let fs = Fs::connect(&link).unwrap();
    std::fs::write(first.join("f.txt"), "moved bytes").unwrap();
    let when = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000);
    std::fs::File::options()
        .write(true)
        .open(first.join("f.txt"))
        .unwrap()
        .set_modified(when)
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(first.join("f.txt"), std::fs::Permissions::from_mode(0o640))
            .unwrap();
    }
    let from = first.join("f.txt").to_string_lossy().into_owned();
    let to = second.join("f.txt").to_string_lossy().into_owned();
    fs.move_to(&from, &to, false).unwrap();
    assert!(!first.join("f.txt").exists());
    assert_eq!(text(&second.join("f.txt")), "moved bytes");
    assert_eq!(
        std::fs::metadata(second.join("f.txt"))
            .unwrap()
            .modified()
            .unwrap(),
        when
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(second.join("f.txt"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o640);
    }
    // A whole directory, and a refusal that leaves the original where it was.
    std::fs::create_dir_all(first.join("tree/inner")).unwrap();
    std::fs::write(first.join("tree/inner/a"), "a").unwrap();
    std::fs::write(first.join("tree/b"), "b").unwrap();
    let from = first.join("tree").to_string_lossy().into_owned();
    let to = second.join("tree").to_string_lossy().into_owned();
    fs.move_to(&from, &to, false).unwrap();
    assert!(!first.join("tree").exists());
    assert_eq!(text(&second.join("tree/inner/a")), "a");
    assert_eq!(text(&second.join("tree/b")), "b");
    std::fs::write(first.join("again"), "stays").unwrap();
    std::fs::write(second.join("again"), "there").unwrap();
    let from = first.join("again").to_string_lossy().into_owned();
    let to = second.join("again").to_string_lossy().into_owned();
    assert_eq!(code(fs.move_to(&from, &to, false)), ErrorCode::Exists);
    assert_eq!(text(&first.join("again")), "stays");
    assert_eq!(text(&second.join("again")), "there");
}

#[test]
fn truncate_cuts_and_extends_a_regular_file_and_nothing_else() {
    let f = fixture(FilesystemConfig::default());
    let fs = Fs::connect(&f.link).unwrap();
    std::fs::write(f.root.join("t"), "0123456789").unwrap();
    fs.truncate("t", 4).unwrap();
    assert_eq!(text(&f.root.join("t")), "0123");
    fs.truncate("t", 8).unwrap();
    assert_eq!(std::fs::read(f.root.join("t")).unwrap(), b"0123\0\0\0\0");
    fs.truncate("t", 0).unwrap();
    assert_eq!(std::fs::metadata(f.root.join("t")).unwrap().len(), 0);
    std::fs::create_dir(f.root.join("dir")).unwrap();
    assert_eq!(code(fs.truncate("dir", 1)), ErrorCode::BadRequest);
    assert_eq!(code(fs.truncate("nothing", 1)), ErrorCode::NotFound);
    assert_eq!(code(fs.truncate("../x", 1)), ErrorCode::OutsideRoot);
    // A peer that may only read cannot cut.
    let tmp = tempfile::tempdir().unwrap();
    let grant = Grant::allowing([Permission::FsRead]).with_fs_roots([tmp.path().to_path_buf()]);
    let reading = Link::files(grant, FilesystemConfig::default());
    std::fs::write(tmp.path().join("t"), "keep").unwrap();
    let fs = Fs::connect(&reading).unwrap();
    assert_eq!(code(fs.truncate("t", 0)), ErrorCode::Denied);
    assert_eq!(text(&tmp.path().join("t")), "keep");
}

#[test]
fn a_recursive_copy_makes_the_tree_on_the_host_and_preserve_keeps_times_and_modes() {
    let f = fixture(FilesystemConfig::default());
    let fs = Fs::connect(&f.link).unwrap();
    std::fs::create_dir_all(f.root.join("src/a/b")).unwrap();
    std::fs::write(f.root.join("src/top.txt"), "top").unwrap();
    std::fs::write(f.root.join("src/a/mid.txt"), "mid").unwrap();
    std::fs::write(f.root.join("src/a/b/low.txt"), "low").unwrap();
    std::fs::create_dir(f.root.join("src/empty")).unwrap();
    let when = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_400_000_000);
    std::fs::File::options()
        .write(true)
        .open(f.root.join("src/a/mid.txt"))
        .unwrap()
        .set_modified(when)
        .unwrap();
    let linked = make_link(&f.root.join("src"));
    // Without `recursive` a directory is refused.
    assert_eq!(code(fs.copy("src", "plain", false)), ErrorCode::BadRequest);
    fs.copy_with(
        "src",
        "dst",
        &CopyOptions {
            recursive: true,
            preserve: true,
            ..CopyOptions::default()
        },
    )
    .unwrap();
    assert_eq!(text(&f.root.join("dst/top.txt")), "top");
    assert_eq!(text(&f.root.join("dst/a/mid.txt")), "mid");
    assert_eq!(text(&f.root.join("dst/a/b/low.txt")), "low");
    assert!(f.root.join("dst/empty").is_dir());
    assert_eq!(
        std::fs::metadata(f.root.join("dst/a/mid.txt"))
            .unwrap()
            .modified()
            .unwrap(),
        when
    );
    if linked {
        assert!(
            std::fs::symlink_metadata(f.root.join("dst/link"))
                .unwrap()
                .is_symlink(),
            "a link is copied as a link"
        );
    }
    // The target exists: refused, then merged with `overwrite`.
    let again = CopyOptions {
        recursive: true,
        ..CopyOptions::default()
    };
    assert_eq!(code(fs.copy_with("src", "dst", &again)), ErrorCode::Exists);
    std::fs::write(f.root.join("src/top.txt"), "changed").unwrap();
    fs.copy_with(
        "src",
        "dst",
        &CopyOptions {
            overwrite: true,
            ..again
        },
    )
    .unwrap();
    assert_eq!(text(&f.root.join("dst/top.txt")), "changed");
    // A tree past `max_tree_entries` stops with `busy`.
    let small = fixture(FilesystemConfig {
        max_tree_entries: 3,
        ..FilesystemConfig::default()
    });
    let fs = Fs::connect(&small.link).unwrap();
    std::fs::create_dir(small.root.join("big")).unwrap();
    for index in 0..10 {
        std::fs::write(small.root.join(format!("big/{index}")), "x").unwrap();
    }
    assert_eq!(code(fs.copy_with("big", "copy", &again)), ErrorCode::Busy);
}

/// Makes `link` in `src` pointing at `top.txt` when the operating system lets this process, so the copy has a link to copy.
fn make_link(src: &Path) -> bool {
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink("top.txt", src.join("link"));
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_file("top.txt", src.join("link"));
    match made {
        Ok(()) => true,
        Err(_) => {
            println!("SKIPPED: this process may not make a symbolic link");
            false
        }
    }
}

#[test]
fn metadata_names_the_device_inode_and_links_where_the_host_reports_them() {
    let f = fixture(FilesystemConfig::default());
    let fs = Fs::connect(&f.link).unwrap();
    std::fs::write(f.root.join("one"), "x").unwrap();
    std::fs::hard_link(f.root.join("one"), f.root.join("two")).unwrap();
    std::fs::write(f.root.join("three"), "y").unwrap();
    let one = fs.stat("one", false).unwrap();
    let two = fs.stat("two", false).unwrap();
    let three = fs.stat("three", false).unwrap();
    assert_eq!(one.kind, FileKind::File);
    if cfg!(unix) {
        assert!(one.device.is_some() && one.inode.is_some());
        assert_eq!((one.device, one.inode), (two.device, two.inode));
        assert_ne!(one.inode, three.inode);
        assert_eq!((one.links, three.links), (Some(2), Some(1)));
    } else {
        // Windows reports none through cap-std: `samefile` cannot be told there.
        assert_eq!((one.device, one.inode, one.links), (None, None, None));
    }
}
