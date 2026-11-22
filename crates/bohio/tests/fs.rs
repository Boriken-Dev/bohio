//! Filesystem calls and transfers, served and driven in-process over memory channels.
//!
//! Every refusal has a test: a `..` that escapes, an absolute path outside the roots, a symbolic
//! link out of a root (at the end of a path and in the middle of one), a link swapped in while
//! paths are being read, an absolute link pointing back inside, a peer without the permission, an
//! empty root list and a disabled filesystem. Tests that need a symbolic link print `SKIPPED` and
//! return when the operating system refuses to create one.

#![forbid(unsafe_code)]

mod support;

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use bohio::config::{FilesystemConfig, Span};
use bohio::{Grant, Permission};
use bohio_client::fs::{Fs, Reader, Writer};
use bohio_client::hash::{hash_file, hash_range};
use bohio_client::{Error, Open};
use bohio_proto::fs::{FileKind, ReadRequest, Timestamp, WriteMode, WriteRequest};
use bohio_proto::hash::{Algorithm, Digests, HashItem};
use bohio_proto::{ErrorCode, protocol};
use bohio_transfer::hash::Hasher;
use sha2::{Digest, Sha256};
use support::{Link, WAIT};
use tempfile::TempDir;

const RW: [Permission; 2] = [Permission::FsRead, Permission::FsWrite];
const OUTSIDE: &[u8] = b"OUTSIDE: this file is beyond the root";
const INSIDE: &[u8] = b"INSIDE: this file is under the root";

/// A root with a file beside it that no peer may reach.
struct Fixture {
    _tmp: TempDir,
    root: PathBuf,
    outside: PathBuf,
    link: Link,
}

fn fixture(allow: &[Permission]) -> Fixture {
    fixture_with(allow, FilesystemConfig::default())
}

fn fixture_with(allow: &[Permission], config: FilesystemConfig) -> Fixture {
    let tmp = tempfile::tempdir().expect("a scratch directory");
    let root = tmp.path().join("root");
    let outside = tmp.path().join("outside");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("secret.txt"), OUTSIDE).unwrap();
    let grant = Grant::allowing(allow.iter().copied()).with_fs_roots([root.clone()]);
    Fixture {
        _tmp: tmp,
        root,
        outside,
        link: Link::files(grant, config),
    }
}

impl Fixture {
    fn fs(&self) -> Fs {
        Fs::connect(&self.link)
            .expect("the control stream opens")
            .with_timeout(WAIT)
    }

    fn put(&self, path: &str, bytes: &[u8]) {
        let writer =
            Writer::start(&self.link, &WriteRequest::replace(path), WAIT).expect("write opens");
        writer.write(bytes).unwrap();
        writer.finish(WAIT).expect("write completes");
    }

    fn get(&self, path: &str) -> Result<Vec<u8>, Error> {
        Reader::start(&self.link, &ReadRequest::whole(path), WAIT)?.read_to_end(WAIT)
    }

    fn no_temporary_files(&self) {
        let left: Vec<_> = std::fs::read_dir(&self.root)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(".bohio-"))
            .collect();
        assert!(left.is_empty(), "temporary files left behind: {left:?}");
    }

    fn outside_is_untouched(&self) {
        assert_eq!(
            std::fs::read(self.outside.join("secret.txt")).unwrap(),
            OUTSIDE
        );
        let names: Vec<_> = std::fs::read_dir(&self.outside)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect();
        assert_eq!(
            names.len(),
            1,
            "something was created outside the root: {names:?}"
        );
    }
}

fn code<T: std::fmt::Debug>(result: Result<T, Error>) -> ErrorCode {
    match result {
        Err(Error::Remote(body)) => body.code,
        other => panic!("expected the host to refuse, got {other:?}"),
    }
}

/// Makes a symbolic link; `false` (after saying so) when the operating system refuses.
fn make_symlink(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let made = if target.is_dir() || target.extension().is_none() {
        std::os::windows::fs::symlink_dir(target, link)
    } else {
        std::os::windows::fs::symlink_file(target, link)
    };
    match made {
        Ok(()) => true,
        Err(error) => {
            eprintln!("SKIPPED: the operating system refuses to create a symbolic link: {error}");
            false
        }
    }
}

/// The host's SHA-256 of `path`, over a `bohio-hash/1` stream.
fn sha256_of(fixture: &Fixture, path: &str) -> String {
    hash_file(&fixture.link, path, &[Algorithm::Sha256]).unwrap()[&Algorithm::Sha256].clone()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut text = String::new();
    for byte in Sha256::digest(bytes).iter() {
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

#[test]
fn list_stat_write_and_read_round_trip() {
    let fixture = fixture(&RW);
    let fs = fixture.fs();
    fs.mkdir("docs", false).unwrap();
    fixture.put("docs/a.txt", b"alpha");
    fixture.put("docs/b.bin", &[0, 1, 2, 255]);

    let names: Vec<_> = fs
        .list_all("docs")
        .unwrap()
        .into_iter()
        .map(|entry| (entry.name, entry.metadata.kind, entry.metadata.size))
        .collect();
    assert_eq!(
        names,
        [
            ("a.txt".to_owned(), FileKind::File, 5),
            ("b.bin".to_owned(), FileKind::File, 4)
        ]
    );
    let meta = fs.stat("docs/a.txt", false).unwrap();
    assert_eq!((meta.kind, meta.size), (FileKind::File, 5));
    assert!(meta.modified.is_some());
    assert_eq!(fs.stat("docs", false).unwrap().kind, FileKind::Directory);
    assert_eq!(fixture.get("docs/a.txt").unwrap(), b"alpha");
    assert_eq!(fixture.get("docs/b.bin").unwrap(), [0, 1, 2, 255]);
    assert_eq!(
        std::fs::read(fixture.root.join("docs").join("a.txt")).unwrap(),
        b"alpha"
    );
    assert_eq!(code(fs.stat("docs/missing", false)), ErrorCode::NotFound);
    assert_eq!(code(fixture.get("docs/missing")), ErrorCode::NotFound);
    drop(fs);
    let served = fixture.link.finished_fs();
    assert_eq!(
        served.iter().filter(|r| r.is_err()).count(),
        1,
        "only the read of a missing file failed: {served:?}"
    );
    fixture.no_temporary_files();
}

#[test]
fn an_absolute_path_under_the_root_is_the_same_file() {
    let fixture = fixture(&RW);
    fixture.put("x.txt", b"x");
    let absolute = fixture.root.join("x.txt").to_string_lossy().into_owned();
    assert_eq!(fixture.get(&absolute).unwrap(), b"x");
    assert_eq!(fixture.fs().stat(&absolute, false).unwrap().size, 1);
}

#[test]
fn reads_honour_offset_and_length() {
    let fixture = fixture(&RW);
    fixture.put("n.txt", b"0123456789");
    let part = |offset, length| {
        let request = ReadRequest {
            path: "n.txt".into(),
            offset,
            length,
            ..ReadRequest::whole("n.txt")
        };
        let reader = Reader::start(&fixture.link, &request, WAIT)?;
        let header = reader.header();
        reader.read_to_end(WAIT).map(|bytes| (header, bytes))
    };
    let (header, bytes) = part(3, Some(4)).unwrap();
    assert_eq!(
        (header.size, header.length, bytes.as_slice()),
        (10, 4, b"3456".as_slice())
    );
    assert_eq!(part(8, Some(100)).unwrap().1, b"89");
    assert_eq!(part(10, None).unwrap().1, b"");
    assert_eq!(code(part(11, None)), ErrorCode::BadRequest);
    assert_eq!(code(fixture.get(".")), ErrorCode::BadRequest);
    fixture.fs().mkdir("dir", false).unwrap();
    assert_eq!(code(fixture.get("dir")), ErrorCode::BadRequest);
}

/// Sets a flag when dropped, so a helper thread that loops until it is set still ends when the
/// test body fails: a scope otherwise waits for that thread forever.
struct StopOnDrop<'a>(&'a AtomicBool);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Resident memory of this process, sampled while `work` runs.
fn peak_growth_during(work: impl FnOnce()) -> u64 {
    use sysinfo::{Pid, ProcessesToUpdate, System};
    let pid = Pid::from_u32(std::process::id());
    let sample = |system: &mut System| {
        system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
        system.process(pid).map_or(0, sysinfo::Process::memory)
    };
    let mut system = System::new();
    let baseline = sample(&mut system);
    let stop = AtomicBool::new(false);
    let mut peak = baseline;
    std::thread::scope(|scope| {
        let sampler = scope.spawn(|| {
            let mut system = System::new();
            let mut peak = baseline;
            while !stop.load(Ordering::Acquire) {
                peak = peak.max(sample(&mut system));
                std::thread::sleep(Duration::from_millis(5));
            }
            peak.max(sample(&mut system))
        });
        {
            let _stop = StopOnDrop(&stop);
            work();
        }
        peak = sampler.join().unwrap();
    });
    peak.saturating_sub(baseline)
}

#[test]
fn a_64_mib_file_streams_in_both_directions_with_bounded_memory() {
    const SIZE: u64 = 64 * 1024 * 1024;
    const CHUNK: usize = 1024 * 1024;
    const BOUND: u64 = 16 * 1024 * 1024;
    let fixture = fixture(&RW);
    let source = fixture.root.join("big.bin");
    let mut local = Hasher::new(&[Algorithm::Sha256]).unwrap();
    {
        let mut file = std::io::BufWriter::new(std::fs::File::create(&source).unwrap());
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut chunk = vec![0_u8; CHUNK];
        for _ in 0..SIZE / CHUNK as u64 {
            for byte in &mut chunk {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = state.to_le_bytes()[0];
            }
            local.update(&chunk);
            file.write_all(&chunk).unwrap();
        }
    }
    let want = local.finish()[&Algorithm::Sha256].clone();

    // One small exchange first, so threads and buffers the engines always need are not counted.
    fixture.put("warm.txt", b"warm");
    assert_eq!(fixture.get("warm.txt").unwrap(), b"warm");

    let mut read_hash = Hasher::new(&[Algorithm::Sha256]).unwrap();
    let mut read_total = 0_u64;
    let read_growth = peak_growth_during(|| {
        let reader = Reader::start(&fixture.link, &ReadRequest::whole("big.bin"), WAIT).unwrap();
        assert_eq!(reader.header().length, SIZE);
        while let Some(chunk) = reader.next_chunk(WAIT).unwrap() {
            read_hash.update(&chunk);
            read_total += chunk.len() as u64;
        }
    });
    assert_eq!(read_total, SIZE);
    assert_eq!(read_hash.finish()[&Algorithm::Sha256], want);
    assert!(
        read_growth < BOUND,
        "reading grew memory by {read_growth} bytes"
    );

    let write_growth = peak_growth_during(|| {
        let writer =
            Writer::start(&fixture.link, &WriteRequest::replace("copy.bin"), WAIT).unwrap();
        let mut file = std::fs::File::open(&source).unwrap();
        let mut chunk = vec![0_u8; CHUNK];
        loop {
            let count = file.read(&mut chunk).unwrap();
            if count == 0 {
                break;
            }
            writer.write(&chunk[..count]).unwrap();
        }
        let done = writer.finish(WAIT).unwrap();
        assert_eq!(
            (done.written, done.hashes[&Algorithm::Sha256].as_str()),
            (SIZE, want.as_str())
        );
    });
    assert!(
        write_growth < BOUND,
        "writing grew memory by {write_growth} bytes"
    );
    assert_eq!(sha256_of(&fixture, "copy.bin"), want);
    assert_eq!(
        std::fs::metadata(fixture.root.join("copy.bin"))
            .unwrap()
            .len(),
        SIZE
    );
    eprintln!("64 MiB: read grew resident memory by {read_growth} bytes, write by {write_growth}");
}

#[test]
fn a_hash_matches_a_local_sha256_whole_and_by_range() {
    let fixture = fixture(&RW);
    let data: Vec<u8> = (0..200_000_u32).map(|n| (n % 251) as u8).collect();
    fixture.put("data.bin", &data);
    let sha256 = [Algorithm::Sha256];
    let range = |offset: u64, length: Option<u64>| {
        let item = HashItem {
            offset,
            length,
            ..HashItem::whole("data.bin")
        };
        hash_range(&fixture.link, item, &sha256).map(|digests| digests[&Algorithm::Sha256].clone())
    };
    assert_eq!(sha256_of(&fixture, "data.bin"), sha256_hex(&data));
    assert_eq!(
        range(1000, Some(70_000)).unwrap(),
        sha256_hex(&data[1000..71_000])
    );
    assert_eq!(
        range(199_990, Some(100)).unwrap(),
        sha256_hex(&data[199_990..])
    );
    assert_eq!(range(200_000, None).unwrap(), sha256_hex(b""));
    assert_eq!(code(range(500_000, None)), ErrorCode::BadRequest);
    assert_eq!(
        code(hash_file(&fixture.link, "absent", &sha256)),
        ErrorCode::NotFound
    );
}

#[test]
fn a_dotdot_that_escapes_is_outside_root_for_every_operation() {
    let fixture = fixture(&RW);
    fixture.put("inside.txt", INSIDE);
    let fs = fixture.fs();
    let absolute_outside = fixture
        .outside
        .join("secret.txt")
        .to_string_lossy()
        .into_owned();
    let through_root = format!("{}/../outside/secret.txt", fixture.root.display());
    for path in [
        "../outside/secret.txt",
        "a/../../outside/secret.txt",
        "..",
        absolute_outside.as_str(),
        through_root.as_str(),
    ] {
        let at = |what: &str| format!("{what} {path}");
        assert_eq!(
            code(fs.stat(path, false)),
            ErrorCode::OutsideRoot,
            "{}",
            at("stat")
        );
        assert_eq!(
            code(fs.list(path, None, None)),
            ErrorCode::OutsideRoot,
            "{}",
            at("list")
        );
        assert_eq!(
            code(hash_file(&fixture.link, path, &[Algorithm::Sha256])),
            ErrorCode::OutsideRoot,
            "{}",
            at("hash")
        );
        assert_eq!(
            code(fs.readlink(path)),
            ErrorCode::OutsideRoot,
            "{}",
            at("readlink")
        );
        assert_eq!(
            code(fs.mkdir(path, true)),
            ErrorCode::OutsideRoot,
            "{}",
            at("mkdir")
        );
        assert_eq!(
            code(fs.remove(path, true)),
            ErrorCode::OutsideRoot,
            "{}",
            at("remove")
        );
        assert_eq!(
            code(fs.rename(path, "moved", true)),
            ErrorCode::OutsideRoot,
            "{}",
            at("rename from")
        );
        assert_eq!(
            code(fs.rename("inside.txt", path, true)),
            ErrorCode::OutsideRoot,
            "{}",
            at("rename to")
        );
        assert_eq!(
            code(fs.copy(path, "copied", true)),
            ErrorCode::OutsideRoot,
            "{}",
            at("copy from")
        );
        assert_eq!(
            code(fs.copy("inside.txt", path, true)),
            ErrorCode::OutsideRoot,
            "{}",
            at("copy to")
        );
        assert_eq!(
            code(fs.symlink("t", path)),
            ErrorCode::OutsideRoot,
            "{}",
            at("symlink at")
        );
        assert_eq!(
            code(fs.set_times(path, None, Some(Timestamp { secs: 1, nanos: 0 }))),
            ErrorCode::OutsideRoot,
            "{}",
            at("set_times")
        );
        assert_eq!(
            code(fs.set_mode(path, 0o600)),
            ErrorCode::OutsideRoot,
            "{}",
            at("set_mode")
        );
        assert_eq!(
            code(fs.set_readonly(path, true)),
            ErrorCode::OutsideRoot,
            "{}",
            at("set_readonly")
        );
        assert_eq!(
            code(fixture.get(path)),
            ErrorCode::OutsideRoot,
            "{}",
            at("read")
        );
        let write = Writer::start(&fixture.link, &WriteRequest::replace(path), WAIT);
        assert_eq!(code(write), ErrorCode::OutsideRoot, "{}", at("write"));
    }
    assert_eq!(
        fs.stat("inside.txt", false).unwrap().size,
        INSIDE.len() as u64
    );
    fixture.outside_is_untouched();
    assert!(!fixture.root.join("moved").exists() && !fixture.root.join("copied").exists());
}

#[test]
fn a_symlink_out_of_the_root_is_refused_at_the_end_and_in_the_middle_of_a_path() {
    let fixture = fixture(&RW);
    if !make_symlink(
        &fixture.outside.join("secret.txt"),
        &fixture.root.join("file_link"),
    ) {
        return;
    }
    assert!(make_symlink(
        &fixture.outside,
        &fixture.root.join("dir_link")
    ));
    let fs = fixture.fs();

    // The link at the end of the path.
    assert_eq!(code(fixture.get("file_link")), ErrorCode::OutsideRoot);
    assert_eq!(code(fs.stat("file_link", true)), ErrorCode::OutsideRoot);
    assert_eq!(
        code(hash_file(&fixture.link, "file_link", &[Algorithm::Sha256])),
        ErrorCode::OutsideRoot
    );
    let write = Writer::start(
        &fixture.link,
        &WriteRequest {
            mode: WriteMode::Append,
            ..WriteRequest::replace("file_link")
        },
        WAIT,
    );
    assert_eq!(code(write), ErrorCode::OutsideRoot);
    // The link itself is a link: it can be described and removed without leaving the root.
    assert_eq!(fs.stat("file_link", false).unwrap().kind, FileKind::Symlink);

    // The link in the middle of the path.
    assert_eq!(
        code(fixture.get("dir_link/secret.txt")),
        ErrorCode::OutsideRoot
    );
    assert_eq!(
        code(fs.stat("dir_link/secret.txt", false)),
        ErrorCode::OutsideRoot
    );
    assert_eq!(
        code(fs.list("dir_link", None, None)),
        ErrorCode::OutsideRoot
    );
    assert_eq!(
        code(fs.mkdir("dir_link/made", false)),
        ErrorCode::OutsideRoot
    );
    assert_eq!(
        code(fs.remove("dir_link/secret.txt", false)),
        ErrorCode::OutsideRoot
    );
    assert_eq!(
        code(fs.rename("dir_link/secret.txt", "taken", true)),
        ErrorCode::OutsideRoot
    );
    assert_eq!(
        code(fs.copy("dir_link/secret.txt", "taken", true)),
        ErrorCode::OutsideRoot
    );
    assert_eq!(
        code(fs.copy("file_link", "taken", true)),
        ErrorCode::OutsideRoot
    );
    let write = Writer::start(
        &fixture.link,
        &WriteRequest::replace("dir_link/planted.txt"),
        WAIT,
    );
    assert_eq!(code(write), ErrorCode::OutsideRoot);
    let create = Writer::start(
        &fixture.link,
        &WriteRequest {
            parents: true,
            ..WriteRequest::replace("dir_link/sub/planted.txt")
        },
        WAIT,
    );
    assert_eq!(code(create), ErrorCode::OutsideRoot);

    fixture.outside_is_untouched();
    assert!(!fixture.root.join("taken").exists());
    fixture.no_temporary_files();
}

#[test]
fn removing_a_symlink_removes_the_link_and_never_what_it_points_at() {
    let fixture = fixture(&RW);
    if !make_symlink(&fixture.outside, &fixture.root.join("dir_link")) {
        return;
    }
    let fs = fixture.fs();
    fs.remove("dir_link", true).unwrap();
    assert!(std::fs::symlink_metadata(fixture.root.join("dir_link")).is_err());
    fixture.outside_is_untouched();
}

#[test]
fn recursive_remove_does_not_follow_a_link_inside_the_tree() {
    let fixture = fixture(&RW);
    let fs = fixture.fs();
    fs.mkdir("tree/branch", true).unwrap();
    if !make_symlink(
        &fixture.outside,
        &fixture.root.join("tree").join("branch").join("out"),
    ) {
        return;
    }
    fs.remove("tree", true).unwrap();
    assert!(!fixture.root.join("tree").exists());
    fixture.outside_is_untouched();
}

#[test]
fn an_absolute_symlink_is_refused_even_when_it_points_back_inside_the_root() {
    let fixture = fixture(&RW);
    fixture.put("real.txt", INSIDE);
    if !make_symlink(
        &fixture.root.join("real.txt"),
        &fixture.root.join("absolute_link"),
    ) {
        return;
    }
    assert_eq!(code(fixture.get("absolute_link")), ErrorCode::OutsideRoot);
}

#[test]
fn a_relative_symlink_that_stays_inside_the_root_is_followed() {
    let fixture = fixture(&RW);
    let fs = fixture.fs();
    fs.mkdir("sub", false).unwrap();
    fixture.put("real.txt", INSIDE);
    match fs.symlink("../real.txt", "sub/link") {
        Ok(()) => {}
        Err(Error::Remote(body))
            if body.code == ErrorCode::Denied || body.code == ErrorCode::Io =>
        {
            eprintln!("SKIPPED: the operating system refuses to create a symbolic link: {body}");
            return;
        }
        Err(other) => panic!("{other:?}"),
    }
    assert_eq!(
        fs.readlink("sub/link").unwrap().replace('\\', "/"),
        "../real.txt"
    );
    assert_eq!(fixture.get("sub/link").unwrap(), INSIDE);
    assert_eq!(fs.stat("sub/link", false).unwrap().kind, FileKind::Symlink);
    assert_eq!(fs.stat("sub/link", true).unwrap().kind, FileKind::File);
}

/// Each link a peer makes names a place inside the root by its words, yet on Unix a chain of them
/// leads out: `sub/d` names the root (`..`) and `esc` names `sub/d/..`, which the kernel resolves
/// through `sub/d` to the root's parent. That reaches only programs that follow links without a
/// handle; the host's own calls refuse it. Windows takes the `..` of a link's target by its words
/// (`sub\d\..` is `sub`), so there the chain stays inside.
#[test]
fn a_chain_of_links_a_peer_makes_never_leads_the_hosts_calls_out_of_the_root() {
    let fixture = fixture(&RW);
    let fs = fixture.fs();
    fs.mkdir("sub", false).unwrap();
    match fs.symlink("..", "sub/d") {
        Ok(()) => {}
        Err(Error::Remote(body))
            if body.code == ErrorCode::Denied || body.code == ErrorCode::Io =>
        {
            eprintln!("SKIPPED: the operating system refuses to create a symbolic link: {body}");
            return;
        }
        Err(other) => panic!("{other:?}"),
    }
    fs.symlink("sub/d/..", "esc")
        .expect("each link names a place inside by its words");
    let root = std::fs::canonicalize(&fixture.root).unwrap();
    let resolved = std::fs::canonicalize(fixture.root.join("esc")).unwrap();
    assert_eq!(
        resolved.starts_with(&root),
        cfg!(windows),
        "{} against {}",
        resolved.display(),
        root.display()
    );
    // The handle walk resolves the chain one link at a time, so on Unix it climbs and is refused; on
    // Windows the path is refused too (the link was made as a file link, since its target could not
    // be looked at, so it is not a directory on the way).
    let through = code(fs.stat("esc/outside/secret.txt", true));
    assert!(
        through == ErrorCode::OutsideRoot || cfg!(windows),
        "{through:?}"
    );
    assert!(fixture.get("esc/outside/secret.txt").is_err());
    fixture.outside_is_untouched();
}

#[test]
fn a_symlink_call_refuses_an_absolute_target_and_one_that_climbs_out() {
    let fixture = fixture(&RW);
    let fs = fixture.fs();
    fs.mkdir("sub", false).unwrap();
    let absolute = fixture.outside.to_string_lossy().into_owned();
    assert_eq!(
        code(fs.symlink(&absolute, "abs_link")),
        ErrorCode::OutsideRoot
    );
    assert_eq!(
        code(fs.symlink("../../outside", "sub/up_link")),
        ErrorCode::OutsideRoot
    );
    assert_eq!(code(fs.symlink("..", "up_link")), ErrorCode::OutsideRoot);
    assert_eq!(code(fs.symlink("", "empty_link")), ErrorCode::BadRequest);
    for name in ["abs_link", "sub/up_link", "up_link", "empty_link"] {
        assert!(
            std::fs::symlink_metadata(fixture.root.join(name)).is_err(),
            "{name} was made"
        );
    }
}

#[test]
fn a_link_swapped_in_while_paths_are_read_never_leads_outside() {
    let fixture = fixture(&RW);
    std::fs::create_dir(fixture.root.join("real")).unwrap();
    std::fs::write(fixture.root.join("real").join("secret.txt"), INSIDE).unwrap();
    // Probe: can this host make a directory link at all?
    let probe = fixture.root.join("probe");
    if !make_symlink(&fixture.outside, &probe) {
        return;
    }
    std::fs::remove_dir(&probe)
        .or_else(|_| std::fs::remove_file(&probe))
        .unwrap();

    let stop = AtomicBool::new(false);
    let swap = fixture.root.join("swap");
    let (inside_target, outside_target) = (PathBuf::from("real"), PathBuf::from("../outside"));
    let (mut inside_reads, mut refused, mut leaked) = (0_u32, 0_u32, 0_u32);
    std::thread::scope(|scope| {
        let swapper = scope.spawn(|| {
            let mut flip = false;
            while !stop.load(Ordering::Acquire) {
                let target = if flip {
                    &inside_target
                } else {
                    &outside_target
                };
                flip = !flip;
                let staging = swap.with_extension("new");
                let _ = std::fs::remove_file(&staging).or_else(|_| std::fs::remove_dir(&staging));
                if !make_symlink_quiet(target, &staging) {
                    continue;
                }
                if std::fs::rename(&staging, &swap).is_err() {
                    let _ = std::fs::remove_file(&swap).or_else(|_| std::fs::remove_dir(&swap));
                    let _ = std::fs::rename(&staging, &swap);
                }
            }
        });
        let _stop = StopOnDrop(&stop);
        let started = Instant::now();
        let mut rounds = 0_u32;
        while rounds < 400 && started.elapsed() < Duration::from_secs(60) {
            rounds += 1;
            match fixture.get("swap/secret.txt") {
                Ok(bytes) if bytes == INSIDE => inside_reads += 1,
                Ok(bytes) => {
                    leaked += 1;
                    eprintln!(
                        "LEAK: read {} bytes that are not the inside file",
                        bytes.len()
                    );
                }
                Err(Error::Remote(body)) => {
                    assert!(
                        matches!(
                            body.code,
                            ErrorCode::OutsideRoot
                                | ErrorCode::NotFound
                                | ErrorCode::Io
                                | ErrorCode::Denied
                                // macOS answers a link met under `O_NOFOLLOW` with an invalid path.
                                | ErrorCode::BadRequest
                        ),
                        "{body:?}"
                    );
                    refused += 1;
                }
                Err(other) => panic!("{other:?}"),
            }
        }
        stop.store(true, Ordering::Release);
        swapper.join().unwrap();
    });
    eprintln!(
        "link swap: {inside_reads} reads of the inside file, {refused} refusals, {leaked} leaks"
    );
    assert_eq!(
        leaked, 0,
        "a link swapped in between check and use led outside the root"
    );
    fixture.outside_is_untouched();
}

fn make_symlink_quiet(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let made = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let made = std::os::windows::fs::symlink_dir(target, link);
    made.is_ok()
}

#[test]
fn a_peer_with_fs_read_only_cannot_write_in_any_way() {
    let fixture = fixture(&[Permission::FsRead]);
    std::fs::write(fixture.root.join("f.txt"), b"data").unwrap();
    let fs = fixture.fs();
    assert_eq!(fixture.get("f.txt").unwrap(), b"data");
    assert_eq!(fs.stat("f.txt", false).unwrap().size, 4);
    assert_eq!(fs.list_all(".").unwrap().len(), 1);
    assert_eq!(sha256_of(&fixture, "f.txt"), sha256_hex(b"data"));

    assert_eq!(
        code(Writer::start(
            &fixture.link,
            &WriteRequest::replace("new.txt"),
            WAIT
        )),
        ErrorCode::Denied
    );
    assert_eq!(code(fs.mkdir("d", false)), ErrorCode::Denied);
    assert_eq!(code(fs.remove("f.txt", false)), ErrorCode::Denied);
    assert_eq!(code(fs.rename("f.txt", "g.txt", false)), ErrorCode::Denied);
    assert_eq!(code(fs.copy("f.txt", "g.txt", false)), ErrorCode::Denied);
    assert_eq!(code(fs.symlink("f.txt", "l")), ErrorCode::Denied);
    assert_eq!(
        code(fs.set_times("f.txt", None, Some(Timestamp { secs: 5, nanos: 0 }))),
        ErrorCode::Denied
    );
    assert_eq!(code(fs.set_mode("f.txt", 0o600)), ErrorCode::Denied);
    assert_eq!(code(fs.set_readonly("f.txt", true)), ErrorCode::Denied);
    assert_eq!(std::fs::read(fixture.root.join("f.txt")).unwrap(), b"data");
    assert_eq!(std::fs::read_dir(&fixture.root).unwrap().count(), 1);
}

#[test]
fn a_peer_with_fs_write_only_cannot_read() {
    let fixture = fixture(&[Permission::FsWrite]);
    std::fs::write(fixture.root.join("f.txt"), b"data").unwrap();
    let fs = fixture.fs();
    fixture.put("w.txt", b"written");
    assert_eq!(code(fixture.get("f.txt")), ErrorCode::Denied);
    assert_eq!(code(fs.stat("f.txt", false)), ErrorCode::Denied);
    assert_eq!(code(fs.list(".", None, None)), ErrorCode::Denied);
    assert_eq!(code(fs.readlink("f.txt")), ErrorCode::Denied);
    assert_eq!(
        code(hash_file(&fixture.link, "f.txt", &[Algorithm::Sha256])),
        ErrorCode::Denied
    );
    assert_eq!(code(fs.copy("f.txt", "g.txt", false)), ErrorCode::Denied);
}

#[test]
fn a_peer_with_no_root_or_no_permission_or_a_disabled_filesystem_gets_nothing() {
    let none = fixture(&[]);
    let fs = none.fs();
    assert_eq!(code(fs.stat("x", false)), ErrorCode::Denied);
    assert_eq!(code(none.get("x")), ErrorCode::Denied);
    assert_eq!(
        code(Writer::start(&none.link, &WriteRequest::replace("x"), WAIT)),
        ErrorCode::Denied
    );

    let grant = Grant::allowing(RW);
    let rootless = Link::files(grant, FilesystemConfig::default());
    let fs = Fs::connect(&rootless).unwrap().with_timeout(WAIT);
    assert_eq!(code(fs.stat(".", false)), ErrorCode::Denied);
    assert_eq!(code(fs.mkdir("x", false)), ErrorCode::Denied);

    let off = fixture_with(
        &RW,
        FilesystemConfig {
            enabled: false,
            ..FilesystemConfig::default()
        },
    );
    assert_eq!(code(off.fs().stat(".", false)), ErrorCode::Unsupported);
    assert_eq!(code(off.get("x")), ErrorCode::Unsupported);
    assert_eq!(
        code(Writer::start(&off.link, &WriteRequest::replace("x"), WAIT)),
        ErrorCode::Unsupported
    );
}

#[test]
fn replace_leaves_the_old_file_until_the_data_is_complete_and_cleans_up_when_abandoned() {
    let fixture = fixture(&RW);
    fixture.put("doc.txt", b"old content");

    // A stream closed before its end leaves the target alone and removes the temporary file.
    let writer = Writer::start(&fixture.link, &WriteRequest::replace("doc.txt"), WAIT).unwrap();
    writer.write(b"new, but never finished").unwrap();
    assert_eq!(
        std::fs::read(fixture.root.join("doc.txt")).unwrap(),
        b"old content"
    );
    drop(writer);
    let served = fixture.link.finished_fs();
    assert_eq!(served.len(), 2, "{served:?}");
    assert_eq!(
        served.iter().filter(|r| r.is_err()).count(),
        1,
        "{served:?}"
    );
    assert_eq!(
        std::fs::read(fixture.root.join("doc.txt")).unwrap(),
        b"old content"
    );
    fixture.no_temporary_files();

    // A complete one replaces it in one step.
    fixture.put("doc.txt", b"new content");
    assert_eq!(
        std::fs::read(fixture.root.join("doc.txt")).unwrap(),
        b"new content"
    );
    fixture.no_temporary_files();
}

#[test]
fn a_digest_that_does_not_match_puts_nothing_in_place() {
    let fixture = fixture(&RW);
    fixture.put("doc.txt", b"old");
    let wrong = sha256_hex(b"something else");
    let request = WriteRequest {
        expect: Digests::from([(Algorithm::Sha256, wrong)]),
        ..WriteRequest::replace("doc.txt")
    };
    let writer = Writer::start(&fixture.link, &request, WAIT).unwrap();
    writer.write(b"new").unwrap();
    assert_eq!(code(writer.finish(WAIT)), ErrorCode::Io);
    assert_eq!(std::fs::read(fixture.root.join("doc.txt")).unwrap(), b"old");
    fixture.no_temporary_files();

    let right = WriteRequest {
        expect: Digests::from([(Algorithm::Sha256, sha256_hex(b"new"))]),
        ..WriteRequest::replace("doc.txt")
    };
    let writer = Writer::start(&fixture.link, &right, WAIT).unwrap();
    writer.write(b"new").unwrap();
    assert_eq!(
        writer.finish(WAIT).unwrap().hashes[&Algorithm::Sha256],
        sha256_hex(b"new")
    );
    assert_eq!(std::fs::read(fixture.root.join("doc.txt")).unwrap(), b"new");

    let append_with_sum = WriteRequest {
        mode: WriteMode::Append,
        expect: Digests::from([(Algorithm::Sha256, sha256_hex(b"x"))]),
        ..WriteRequest::replace("doc.txt")
    };
    assert_eq!(
        code(Writer::start(&fixture.link, &append_with_sum, WAIT)),
        ErrorCode::BadRequest
    );
    let bad_text = WriteRequest {
        expect: Digests::from([(Algorithm::Sha256, "ABC".into())]),
        ..WriteRequest::replace("doc.txt")
    };
    assert_eq!(
        code(Writer::start(&fixture.link, &bad_text, WAIT)),
        ErrorCode::BadRequest
    );
}

#[test]
fn create_append_and_at_write_where_they_say() {
    let fixture = fixture(&RW);
    let mode = |mode| WriteRequest {
        mode,
        ..WriteRequest::replace("m.txt")
    };
    let send = |request: &WriteRequest, bytes: &[u8]| {
        let writer = Writer::start(&fixture.link, request, WAIT)?;
        writer.write(bytes)?;
        writer.finish(WAIT)
    };
    send(&mode(WriteMode::Create), b"abcdef").unwrap();
    assert_eq!(
        code(send(&mode(WriteMode::Create), b"zzz")),
        ErrorCode::Exists
    );
    assert_eq!(
        std::fs::read(fixture.root.join("m.txt")).unwrap(),
        b"abcdef"
    );
    send(&mode(WriteMode::Append), b"GH").unwrap();
    assert_eq!(
        std::fs::read(fixture.root.join("m.txt")).unwrap(),
        b"abcdefGH"
    );
    send(&mode(WriteMode::At { offset: 2 }), b"XY").unwrap();
    assert_eq!(
        std::fs::read(fixture.root.join("m.txt")).unwrap(),
        b"abXYefGH"
    );
    send(
        &WriteRequest {
            mode: WriteMode::At { offset: 0 },
            ..WriteRequest::replace("fresh.txt")
        },
        b"hi",
    )
    .unwrap();
    assert_eq!(
        std::fs::read(fixture.root.join("fresh.txt")).unwrap(),
        b"hi"
    );

    // A `create` abandoned part way removes the file it made.
    let abandoned = WriteRequest {
        path: "gone.txt".into(),
        mode: WriteMode::Create,
        ..WriteRequest::replace("gone.txt")
    };
    let writer = Writer::start(&fixture.link, &abandoned, WAIT).unwrap();
    writer.write(b"partial").unwrap();
    drop(writer);
    fixture.link.finished_fs();
    assert!(!fixture.root.join("gone.txt").exists());
}

#[test]
fn writes_refuse_a_directory_and_a_root_and_create_parents_only_when_asked() {
    let fixture = fixture(&RW);
    fixture.fs().mkdir("dir", false).unwrap();
    assert_eq!(
        code(Writer::start(
            &fixture.link,
            &WriteRequest::replace("dir"),
            WAIT
        )),
        ErrorCode::BadRequest
    );
    assert_eq!(
        code(Writer::start(
            &fixture.link,
            &WriteRequest::replace("."),
            WAIT
        )),
        ErrorCode::BadRequest
    );
    assert_eq!(
        code(Writer::start(
            &fixture.link,
            &WriteRequest::replace("no/such/dir/f"),
            WAIT
        )),
        ErrorCode::NotFound
    );
    let with_parents = WriteRequest {
        parents: true,
        ..WriteRequest::replace("no/such/dir/f")
    };
    let writer = Writer::start(&fixture.link, &with_parents, WAIT).unwrap();
    writer.write(b"deep").unwrap();
    writer.finish(WAIT).unwrap();
    assert_eq!(
        std::fs::read(fixture.root.join("no").join("such").join("dir").join("f")).unwrap(),
        b"deep"
    );
    fixture.no_temporary_files();
}

#[test]
fn directories_rename_copy_and_remove_with_their_errors() {
    let fixture = fixture(&RW);
    let fs = fixture.fs();
    fs.mkdir("a/b", true).unwrap();
    assert_eq!(code(fs.mkdir("a", false)), ErrorCode::Exists);
    fs.mkdir("a", true).unwrap();
    assert_eq!(code(fs.mkdir("x/y", false)), ErrorCode::NotFound);
    fixture.put("a/b/f.txt", b"payload");

    fs.copy("a/b/f.txt", "a/copy.txt", false).unwrap();
    assert_eq!(fixture.get("a/copy.txt").unwrap(), b"payload");
    assert_eq!(
        code(fs.copy("a/b/f.txt", "a/copy.txt", false)),
        ErrorCode::Exists
    );
    fixture.put("a/copy.txt", b"changed");
    fs.copy("a/b/f.txt", "a/copy.txt", true).unwrap();
    assert_eq!(fixture.get("a/copy.txt").unwrap(), b"payload");
    assert_eq!(
        code(fs.copy("a/b", "a/dircopy", false)),
        ErrorCode::BadRequest
    );
    assert_eq!(
        code(fs.copy("a/missing", "a/z", false)),
        ErrorCode::NotFound
    );

    fs.rename("a/copy.txt", "a/renamed.txt", false).unwrap();
    assert_eq!(code(fs.stat("a/copy.txt", false)), ErrorCode::NotFound);
    assert_eq!(
        code(fs.rename("a/renamed.txt", "a/b/f.txt", false)),
        ErrorCode::Exists
    );
    fs.rename("a/renamed.txt", "a/b/f.txt", true).unwrap();
    assert_eq!(fixture.get("a/b/f.txt").unwrap(), b"payload");

    assert_eq!(code(fs.remove("a", false)), ErrorCode::BadRequest);
    fs.remove("a", true).unwrap();
    assert_eq!(code(fs.remove("a", true)), ErrorCode::NotFound);
    assert_eq!(code(fs.remove(".", true)), ErrorCode::BadRequest);
    assert_eq!(
        code(fs.remove(&fixture.root.to_string_lossy(), true)),
        ErrorCode::BadRequest
    );
    assert!(fixture.root.is_dir());
    fixture.no_temporary_files();
}

#[test]
fn times_modes_and_attributes_follow_the_host() {
    let fixture = fixture(&RW);
    let fs = fixture.fs();
    fixture.put("t.txt", b"t");
    let modified = Timestamp {
        secs: 1_600_000_000,
        nanos: 500_000_000,
    };
    let accessed = Timestamp {
        secs: 1_500_000_000,
        nanos: 0,
    };
    fs.set_times("t.txt", Some(accessed), Some(modified))
        .unwrap();
    let meta = fs.stat("t.txt", false).unwrap();
    assert_eq!(meta.modified, Some(modified));
    assert_eq!(
        code(fs.set_times(
            "t.txt",
            None,
            Some(Timestamp {
                secs: 0,
                nanos: 2_000_000_000
            })
        )),
        ErrorCode::BadRequest
    );

    #[cfg(unix)]
    {
        fs.set_mode("t.txt", 0o640).unwrap();
        assert_eq!(fs.stat("t.txt", false).unwrap().mode, Some(0o640));
        assert_eq!(code(fs.set_mode("t.txt", 0o17_777)), ErrorCode::BadRequest);
        assert_eq!(code(fs.set_readonly("t.txt", true)), ErrorCode::Unsupported);
        let request = WriteRequest {
            file_mode: Some(0o600),
            ..WriteRequest::replace("private.txt")
        };
        let writer = Writer::start(&fixture.link, &request, WAIT).unwrap();
        writer.write(b"p").unwrap();
        writer.finish(WAIT).unwrap();
        assert_eq!(fs.stat("private.txt", false).unwrap().mode, Some(0o600));
        // A replaced file keeps its mode.
        fixture.put("private.txt", b"again");
        assert_eq!(fs.stat("private.txt", false).unwrap().mode, Some(0o600));
    }
    #[cfg(windows)]
    {
        assert_eq!(code(fs.set_mode("t.txt", 0o600)), ErrorCode::Unsupported);
        assert_eq!(fs.stat("t.txt", false).unwrap().mode, None);
        fs.set_readonly("t.txt", true).unwrap();
        let meta = fs.stat("t.txt", false).unwrap();
        assert!(meta.readonly && meta.attributes.is_some());
        fs.set_readonly("t.txt", false).unwrap();
        assert!(!fs.stat("t.txt", false).unwrap().readonly);
    }
}

/// A file a peer may change runs as the service's account for whoever starts it when it carries the
/// set-user-ID bit, so a peer that may only write files must not be able to set it, or set-group-ID.
#[cfg(unix)]
#[test]
fn a_peer_cannot_make_a_file_or_directory_set_user_or_group_id() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = fixture(&RW);
    let fs = fixture.fs();
    fixture.put("tool", b"#!/bin/sh\nid\n");
    for mode in [0o4755, 0o2755, 0o6755] {
        assert_eq!(
            code(fs.set_mode("tool", mode)),
            ErrorCode::Denied,
            "{mode:o}"
        );
    }
    let mode_of = |name: &str| {
        std::fs::metadata(fixture.root.join(name))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777
    };
    assert_eq!(mode_of("tool") & 0o6000, 0);

    let request = WriteRequest {
        file_mode: Some(0o4755),
        ..WriteRequest::replace("written")
    };
    let refused = Writer::start(&fixture.link, &request, WAIT)
        .and_then(|writer| {
            writer.write(b"x")?;
            writer.finish(WAIT)
        })
        .map(|_| ());
    assert_eq!(code(refused), ErrorCode::Denied);
    assert!(!fixture.root.join("written").exists());

    let made = fs.control().call(
        bohio_proto::control::Call::Mkdir {
            path: "shared".to_owned(),
            parents: false,
            mode: Some(0o2775),
        },
        WAIT,
    );
    assert_eq!(code(made), ErrorCode::Denied);
    assert!(!fixture.root.join("shared").exists());

    // A file the service account already holds with the bits keeps none of them in a copy or a
    // replacement a peer makes.
    std::fs::set_permissions(
        fixture.root.join("tool"),
        std::fs::Permissions::from_mode(0o4755),
    )
    .unwrap();
    fs.copy("tool", "copied", false).unwrap();
    assert_eq!(mode_of("copied") & 0o6000, 0, "the copy is set-user-ID");
    fixture.put("tool", b"#!/bin/sh\necho replaced\n");
    assert_eq!(
        mode_of("tool") & 0o6000,
        0,
        "the replacement is set-user-ID"
    );
    fs.set_mode("tool", 0o750).unwrap();
    assert_eq!(mode_of("tool"), 0o750);
}

#[test]
fn a_listing_is_paged_sorted_and_bounded_by_the_host() {
    let config = FilesystemConfig {
        list_page: 7,
        ..FilesystemConfig::default()
    };
    let fixture = fixture_with(&RW, config);
    let fs = fixture.fs();
    for n in 0..25 {
        std::fs::write(fixture.root.join(format!("f{n:02}")), b"x").unwrap();
    }
    let first = fs.list(".", None, Some(1000)).unwrap();
    assert_eq!(
        first.entries.len(),
        7,
        "the host's page bounds what the client asks for"
    );
    assert_eq!(first.entries[0].name, "f00");
    assert_eq!(first.next.as_deref(), Some("f06"));
    let small = fs.list(".", Some("f06"), Some(3)).unwrap();
    assert_eq!(
        small
            .entries
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>(),
        ["f07", "f08", "f09"]
    );
    let all: Vec<_> = fs
        .list_all(".")
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    let expected: Vec<_> = (0..25).map(|n| format!("f{n:02}")).collect();
    assert_eq!(all, expected);
    let last = fs.list(".", Some("f23"), None).unwrap();
    assert_eq!((last.entries.len(), last.next), (1, None));
    assert_eq!(fs.list(".", None, Some(0)).unwrap().entries.len(), 1);
    assert_eq!(code(fs.list("f00", None, None)), ErrorCode::BadRequest);
}

// macOS file systems refuse a name that is not valid text, so there is none to list.
#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn a_name_that_is_not_text_is_counted_and_left_out() {
    use std::os::unix::ffi::OsStrExt;
    let fixture = fixture(&RW);
    std::fs::write(fixture.root.join("ok.txt"), b"x").unwrap();
    std::fs::write(
        fixture
            .root
            .join(std::ffi::OsStr::from_bytes(b"bad\xff.txt")),
        b"x",
    )
    .unwrap();
    let listing = fixture.fs().list(".", None, None).unwrap();
    assert_eq!(listing.entries.len(), 1);
    assert_eq!(listing.skipped, 1);
}

#[test]
fn several_roots_a_relative_path_uses_the_first_and_an_absolute_one_its_own() {
    let tmp = tempfile::tempdir().unwrap();
    let (one, two) = (tmp.path().join("one"), tmp.path().join("two"));
    std::fs::create_dir(&one).unwrap();
    std::fs::create_dir(&two).unwrap();
    std::fs::write(one.join("f.txt"), b"first").unwrap();
    std::fs::write(two.join("f.txt"), b"second").unwrap();
    let grant = Grant::allowing(RW).with_fs_roots([one.clone(), two.clone()]);
    let link = Link::files(grant, FilesystemConfig::default());
    let get = |path: &str| {
        Reader::start(&link, &ReadRequest::whole(path), WAIT).and_then(|r| r.read_to_end(WAIT))
    };
    assert_eq!(get("f.txt").unwrap(), b"first");
    assert_eq!(
        get(&two.join("f.txt").to_string_lossy()).unwrap(),
        b"second"
    );
    assert_eq!(
        code(get(&tmp.path().join("f.txt").to_string_lossy())),
        ErrorCode::OutsideRoot
    );
    assert_eq!(code(get("../two/f.txt")), ErrorCode::OutsideRoot);
}

#[cfg(windows)]
#[test]
fn windows_roots_match_without_case_and_refuse_devices_and_streams() {
    let fixture = fixture(&RW);
    fixture.put("File.txt", b"w");
    let upper = fixture
        .root
        .join("File.txt")
        .to_string_lossy()
        .to_uppercase();
    let forward = fixture
        .root
        .join("file.txt")
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase();
    assert_eq!(fixture.get(&upper).unwrap(), b"w");
    assert_eq!(fixture.get(&forward).unwrap(), b"w");
    for bad in [
        "nul",
        "CON.txt",
        "sub\\com1",
        "File.txt:stream",
        "File.txt::$DATA",
        "trail.",
        "\\\\.\\C:\\x",
        "\\\\?\\GLOBALROOT\\x",
        "C:relative",
    ] {
        assert_eq!(code(fixture.get(bad)), ErrorCode::BadRequest, "{bad}");
        assert_eq!(
            code(Writer::start(
                &fixture.link,
                &WriteRequest::replace(bad),
                WAIT
            )),
            ErrorCode::BadRequest,
            "{bad}"
        );
    }
    let other_drive = if fixture
        .root
        .to_string_lossy()
        .to_uppercase()
        .starts_with("C:")
    {
        "D:\\x"
    } else {
        "C:\\x"
    };
    assert_eq!(code(fixture.get(other_drive)), ErrorCode::OutsideRoot);
}

#[cfg(windows)]
#[test]
fn a_windows_unc_root_confines_the_same_way() {
    // The administrative share of this machine is a UNC path to a local directory.
    let tmp = tempfile::tempdir().unwrap();
    let local = tmp.path().to_string_lossy().into_owned();
    let Some((drive, rest)) = local.split_once(":\\") else {
        return;
    };
    let unc = PathBuf::from(format!("\\\\localhost\\{drive}$\\{rest}"));
    if std::fs::read_dir(&unc).is_err() {
        eprintln!(
            "SKIPPED: the administrative share {} is not reachable",
            unc.display()
        );
        return;
    }
    let root = unc.join("root");
    std::fs::create_dir(tmp.path().join("root")).unwrap();
    std::fs::write(tmp.path().join("root").join("f.txt"), b"unc").unwrap();
    std::fs::write(tmp.path().join("secret.txt"), OUTSIDE).unwrap();
    let grant = Grant::allowing(RW).with_fs_roots([root.clone()]);
    let link = Link::files(grant, FilesystemConfig::default());
    let get = |path: &str| {
        Reader::start(&link, &ReadRequest::whole(path), WAIT).and_then(|r| r.read_to_end(WAIT))
    };
    assert_eq!(get("f.txt").unwrap(), b"unc");
    let shouted = root.join("F.TXT").to_string_lossy().to_uppercase();
    assert_eq!(get(&shouted).unwrap(), b"unc");
    assert_eq!(code(get("..\\secret.txt")), ErrorCode::OutsideRoot);
    assert_eq!(
        code(get(&unc.join("secret.txt").to_string_lossy())),
        ErrorCode::OutsideRoot
    );
}

#[test]
fn a_third_transfer_is_busy_at_max_transfers_and_the_slot_is_freed_after() {
    let config = FilesystemConfig {
        max_transfers: 1,
        ..FilesystemConfig::default()
    };
    let fixture = fixture_with(&RW, config);
    let big = vec![7_u8; 4 * 1024 * 1024];
    std::fs::write(fixture.root.join("big.bin"), &big).unwrap();
    // The first read stays in flight: its stream is full and nobody reads it.
    let held = Reader::start(&fixture.link, &ReadRequest::whole("big.bin"), WAIT).unwrap();
    assert_eq!(
        code(Reader::start(
            &fixture.link,
            &ReadRequest::whole("big.bin"),
            WAIT
        )),
        ErrorCode::Busy
    );
    assert_eq!(
        code(Writer::start(
            &fixture.link,
            &WriteRequest::replace("w.txt"),
            WAIT
        )),
        ErrorCode::Busy
    );
    drop(held);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match Writer::start(&fixture.link, &WriteRequest::replace("w.txt"), WAIT) {
            Ok(writer) => {
                writer.write(b"now").unwrap();
                writer.finish(WAIT).unwrap();
                break;
            }
            Err(Error::Remote(body))
                if body.code == ErrorCode::Busy && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(other) => panic!("{other:?}"),
        }
    }
}

#[test]
fn a_client_that_stops_reading_or_writing_is_abandoned_after_the_idle_timeout() {
    let config = FilesystemConfig {
        idle_timeout: Span(Duration::from_millis(300)),
        poll_interval: Span(Duration::from_millis(10)),
        ..FilesystemConfig::default()
    };
    let fixture = fixture_with(&RW, config);
    std::fs::write(fixture.root.join("big.bin"), vec![1_u8; 4 * 1024 * 1024]).unwrap();
    fixture.put("keep.txt", b"keep");

    let reader = Reader::start(&fixture.link, &ReadRequest::whole("big.bin"), WAIT).unwrap();
    let writer = Writer::start(&fixture.link, &WriteRequest::replace("keep.txt"), WAIT).unwrap();
    writer.write(b"half").unwrap();
    // Neither client moves; the host gives up on both by itself (the streams stay open here).
    let served = fixture.link.finished_fs();
    assert_eq!(
        served.iter().filter(|r| r.is_err()).count(),
        2,
        "{served:?}"
    );
    drop((reader, writer));
    assert_eq!(
        std::fs::read(fixture.root.join("keep.txt")).unwrap(),
        b"keep"
    );
    fixture.no_temporary_files();
}

#[test]
fn the_control_stream_answers_hello_info_and_unreadable_requests() {
    let fixture = fixture(&[Permission::Info]);
    let fs = fixture.fs();
    let control = fs.control();
    assert_eq!(control.hello("test", WAIT).unwrap(), 1);
    assert_eq!(control.info(WAIT).unwrap().protocol, 1);
    assert_eq!(
        code(control.call(
            bohio_proto::control::Call::Hello {
                version: 0,
                client: String::new()
            },
            WAIT
        )),
        ErrorCode::BadRequest
    );
    let denied = fixture_with(&[], FilesystemConfig::default());
    assert_eq!(code(denied.fs().control().info(WAIT)), ErrorCode::Denied);
    assert_eq!(denied.fs().control().hello("x", WAIT).unwrap(), 1);

    // Bytes that are not a request get an answer with id 0 and the stream goes on.
    let channel = fixture.link.open(protocol::CONTROL, &[]).unwrap();
    channel.send(&[0xff, 0x00, 0x13]).unwrap();
    let reply = channel.receive(WAIT).unwrap().unwrap();
    let response: bohio_proto::control::Response = bohio_proto::decode(&reply).unwrap();
    assert_eq!(
        (response.id, response.error.map(|e| e.code)),
        (0, Some(ErrorCode::BadRequest))
    );
    channel.close();
}

#[test]
fn an_unreadable_open_on_a_transfer_stream_is_a_bad_request() {
    let fixture = fixture(&RW);
    for name in [protocol::READ, protocol::WRITE] {
        let channel = fixture.link.open(name, &[0xff, 0xff]).unwrap();
        let frame = channel.receive(WAIT).unwrap().unwrap();
        assert_eq!(frame[0], 0x05, "{name}: an error frame");
        let body: bohio_proto::ErrorBody = bohio_proto::decode(&frame[1..]).unwrap();
        assert_eq!(body.code, ErrorCode::BadRequest);
        channel.close();
    }
}
