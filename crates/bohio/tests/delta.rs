//! Block signatures, pull and push deltas and the quick compare, served and driven in-process over memory channels.

mod support;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bohio::config::{DeltaConfig, FilesystemConfig};
use bohio::{Grant, Permission};
use bohio_client::fs::{Reader, Writer};
use bohio_client::hash::hash_blocks;
use bohio_client::sync::{
    Compare, DeltaMode, GetOptions, PutOptions, SyncOptions, Verdict, compare_file, get, put,
};
use bohio_client::{Error, Open};
use bohio_proto::compress::{Codec, Compression};
use bohio_proto::fs::{DeltaBase, ReadRequest, WriteMode, WriteRequest};
use bohio_proto::hash::{Algorithm, BlockOptions, Digests, HashItem};
use bohio_proto::{ErrorCode, protocol};
use bohio_transfer::hash::Hasher;
use support::{Link, WAIT};
use tempfile::TempDir;

const READ: [Permission; 1] = [Permission::FsRead];
const RW: [Permission; 2] = [Permission::FsRead, Permission::FsWrite];

struct Fixture {
    _tmp: TempDir,
    /// The host's root.
    root: PathBuf,
    /// The client's directory.
    local: PathBuf,
    link: Link,
}

fn fixture(allow: &[Permission]) -> Fixture {
    fixture_with(allow, DeltaConfig::default())
}

fn fixture_with(allow: &[Permission], delta: DeltaConfig) -> Fixture {
    let tmp = tempfile::tempdir().expect("a scratch directory");
    let root = tmp.path().join("root");
    let local = tmp.path().join("local");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&local).unwrap();
    let grant = Grant::allowing(allow.iter().copied()).with_fs_roots([root.clone()]);
    let mut link = Link::files(grant, FilesystemConfig::default()).deltaing(delta);
    // What the host reports, as the node would: the client reads its delta bounds from it.
    let config = bohio::config::NodeConfig {
        delta: (*link.delta).clone(),
        ..bohio::config::NodeConfig::default()
    };
    link.info = Arc::new(bohio::info::collect(&config, std::ffi::OsStr::new("")));
    Fixture {
        _tmp: tmp,
        root,
        local,
        link,
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

fn sha256(bytes: &[u8]) -> String {
    let mut hasher = Hasher::new(&[Algorithm::Sha256]).unwrap();
    hasher.update(bytes);
    hasher.finish().remove(&Algorithm::Sha256).unwrap()
}

fn code<T: std::fmt::Debug>(result: Result<T, Error>) -> ErrorCode {
    match result {
        Err(Error::Remote(body)) => body.code,
        other => panic!("expected the host to refuse, got {other:?}"),
    }
}

/// Files of the host's root whose name is not one of `names`: a temporary left behind shows here.
fn strays(dir: &Path, names: &[&str]) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !names.contains(&name.as_str()))
        .collect()
}

fn always() -> SyncOptions {
    SyncOptions {
        delta: DeltaMode::Always,
        ..SyncOptions::default()
    }
}

#[test]
fn identical_files_move_no_literal_bytes_both_ways() {
    let fixture = fixture(&RW);
    let data = pseudo(3 << 20, 1);
    std::fs::write(fixture.root.join("f.bin"), &data).unwrap();
    std::fs::write(fixture.local.join("f.bin"), &data).unwrap();
    let pushed = put(
        &fixture.link,
        &fixture.local.join("f.bin"),
        "f.bin",
        &always(),
    )
    .unwrap();
    assert!(pushed.delta);
    assert_eq!(pushed.literal_bytes, 0);
    assert_eq!(pushed.copied_bytes, data.len() as u64);
    assert!(pushed.wire_bytes < 100_000, "{pushed:?}");
    let pulled = get(
        &fixture.link,
        "f.bin",
        &fixture.local.join("f.bin"),
        &always(),
    )
    .unwrap();
    assert!(pulled.delta);
    assert_eq!(pulled.literal_bytes, 0);
    assert!(pulled.wire_bytes < 100_000, "{pulled:?}");
    assert_eq!(std::fs::read(fixture.local.join("f.bin")).unwrap(), data);
    assert_eq!(std::fs::read(fixture.root.join("f.bin")).unwrap(), data);
}

#[test]
fn one_byte_changed_in_64_mib_moves_under_one_percent_both_ways() {
    let fixture = fixture(&RW);
    let size = 64 << 20;
    let old = pseudo(size, 7);
    let mut new = old.clone();
    new[size / 3] ^= 0xff;
    let (host, mine) = (fixture.root.join("big.bin"), fixture.local.join("big.bin"));

    // Push: the host has the old file, the client the new one.
    std::fs::write(&host, &old).unwrap();
    std::fs::write(&mine, &new).unwrap();
    let pushed = put(&fixture.link, &mine, "big.bin", &always()).unwrap();
    assert!(pushed.delta, "{pushed:?}");
    assert!(
        pushed.wire_bytes * 100 < size as u64,
        "pushing moved {} of {size}",
        pushed.wire_bytes
    );
    assert!(pushed.literal_bytes <= 16 * 1024, "{pushed:?}");
    assert_eq!(sha256(&std::fs::read(&host).unwrap()), sha256(&new));

    // Pull: the client has the old file, the host the new one.
    std::fs::write(&mine, &old).unwrap();
    let pulled = get(&fixture.link, "big.bin", &mine, &always()).unwrap();
    assert!(pulled.delta, "{pulled:?}");
    assert!(
        pulled.wire_bytes * 100 < size as u64,
        "pulling moved {} of {size}",
        pulled.wire_bytes
    );
    assert_eq!(sha256(&std::fs::read(&mine).unwrap()), sha256(&new));
    assert!(strays(&fixture.root, &["big.bin"]).is_empty());
    assert!(strays(&fixture.local, &["big.bin"]).is_empty());
}

#[test]
fn a_kilobyte_inserted_at_the_start_finds_the_shifted_blocks() {
    let fixture = fixture(&RW);
    let old = pseudo(2 << 20, 3);
    let mut new = pseudo(1024, 4);
    new.extend_from_slice(&old);
    let (host, mine) = (fixture.root.join("f"), fixture.local.join("f"));
    std::fs::write(&host, &old).unwrap();
    std::fs::write(&mine, &new).unwrap();
    let pushed = put(&fixture.link, &mine, "f", &always()).unwrap();
    assert_eq!(pushed.literal_bytes, 1024, "{pushed:?}");
    assert_eq!(std::fs::read(&host).unwrap(), new);

    std::fs::write(&mine, &old).unwrap();
    let pulled = get(&fixture.link, "f", &mine, &always()).unwrap();
    assert_eq!(pulled.literal_bytes, 1024, "{pulled:?}");
    assert_eq!(std::fs::read(&mine).unwrap(), new);
}

#[test]
fn a_truncated_file_an_appended_one_and_an_empty_base_rebuild() {
    let fixture = fixture(&RW);
    let old = pseudo(1_000_003, 5);
    let (host, mine) = (fixture.root.join("f"), fixture.local.join("f"));
    for (name, new) in [
        ("truncated", old[..400_000].to_vec()),
        ("appended", [old.clone(), pseudo(77_777, 6)].concat()),
        ("emptied", Vec::new()),
    ] {
        std::fs::write(&host, &old).unwrap();
        std::fs::write(&mine, &new).unwrap();
        put(&fixture.link, &mine, "f", &always()).unwrap();
        assert_eq!(std::fs::read(&host).unwrap(), new, "push {name}");
        std::fs::write(&mine, &old).unwrap();
        std::fs::write(&host, &new).unwrap();
        get(&fixture.link, "f", &mine, &always()).unwrap();
        assert_eq!(std::fs::read(&mine).unwrap(), new, "pull {name}");
    }
    // An empty old copy: everything is literal.
    std::fs::write(&host, b"").unwrap();
    std::fs::write(&mine, pseudo(10_000, 9)).unwrap();
    let pushed = put(&fixture.link, &mine, "f", &always()).unwrap();
    assert!(pushed.delta);
    assert_eq!(pushed.literal_bytes, 10_000);
    std::fs::write(&mine, b"").unwrap();
    let pulled = get(&fixture.link, "f", &mine, &always()).unwrap();
    assert!(pulled.delta);
    assert_eq!(pulled.literal_bytes, 10_000);
    assert_eq!(std::fs::read(&mine).unwrap(), std::fs::read(&host).unwrap());
}

#[test]
fn a_missing_destination_is_a_whole_transfer_and_never_is_one_too() {
    let fixture = fixture(&RW);
    let data = pseudo(300_000, 11);
    std::fs::write(fixture.local.join("up"), &data).unwrap();
    let pushed = put(&fixture.link, &fixture.local.join("up"), "up", &always()).unwrap();
    assert!(!pushed.delta);
    assert_eq!(pushed.literal_bytes, 300_000);
    assert_eq!(std::fs::read(fixture.root.join("up")).unwrap(), data);
    let never = SyncOptions {
        delta: DeltaMode::Never,
        ..SyncOptions::default()
    };
    let pulled = get(&fixture.link, "up", &fixture.local.join("down"), &never).unwrap();
    assert!(!pulled.delta);
    assert_eq!(std::fs::read(fixture.local.join("down")).unwrap(), data);
    // Auto uses a delta only from the host's min_size on: 300 kB is below it.
    let auto = get(
        &fixture.link,
        "up",
        &fixture.local.join("down"),
        &GetOptions::default(),
    )
    .unwrap();
    assert!(!auto.delta);
}

#[test]
fn a_base_that_moved_since_the_signature_is_changed_and_the_target_is_untouched() {
    let fixture = fixture(&RW);
    let old = pseudo(200_000, 12);
    let host = fixture.root.join("f");
    std::fs::write(&host, &old).unwrap();
    let seen = hash_blocks(
        &fixture.link,
        HashItem::whole("f"),
        &[Algorithm::Blake3],
        &BlockOptions::default(),
    )
    .unwrap();
    // Someone else rewrites the file after the signature was made.
    let moved = pseudo(200_001, 13);
    std::fs::write(&host, &moved).unwrap();
    let request = WriteRequest {
        mode: WriteMode::Delta {
            block_size: seen.signature.spec().block_size.into(),
            base: DeltaBase {
                size: seen.result.size,
                modified: seen.result.modified,
                hashes: Digests::new(),
            },
        },
        ..WriteRequest::replace("f")
    };
    assert_eq!(
        code(Writer::start(&fixture.link, &request, WAIT)),
        ErrorCode::Changed
    );
    assert_eq!(std::fs::read(&host).unwrap(), moved);
    assert!(strays(&fixture.root, &["f"]).is_empty());

    // The same size, by a different time, is as good: the hashes name the old bytes.
    let request = WriteRequest {
        mode: WriteMode::Delta {
            block_size: 4096,
            base: DeltaBase {
                size: moved.len() as u64,
                modified: None,
                hashes: Digests::from([(Algorithm::Sha256, sha256(&old).replace('0', "1"))]),
            },
        },
        ..WriteRequest::replace("f")
    };
    assert_eq!(
        code(Writer::start(&fixture.link, &request, WAIT)),
        ErrorCode::Changed
    );
}

#[test]
fn a_copy_past_the_end_of_the_target_is_a_bad_request_and_nothing_is_replaced() {
    let fixture = fixture(&RW);
    let old = pseudo(10_000, 14);
    let host = fixture.root.join("f");
    std::fs::write(&host, &old).unwrap();
    let meta = std::fs::metadata(&host).unwrap();
    let _ = meta;
    let seen = hash_blocks(
        &fixture.link,
        HashItem::whole("f"),
        &[Algorithm::Blake3],
        &BlockOptions {
            size: Some(1024),
            ..BlockOptions::default()
        },
    )
    .unwrap();
    let request = WriteRequest {
        mode: WriteMode::Delta {
            block_size: 1024,
            base: DeltaBase {
                size: seen.result.size,
                modified: seen.result.modified,
                hashes: Digests::new(),
            },
        },
        ..WriteRequest::replace("f")
    };
    let writer = Writer::start(&fixture.link, &request, WAIT).unwrap();
    let _ = writer.copy(0, 1);
    let _ = writer.copy(10, 1);
    assert_eq!(code(writer.finish(WAIT)), ErrorCode::BadRequest);
    assert_eq!(std::fs::read(&host).unwrap(), old);
    assert!(strays(&fixture.root, &["f"]).is_empty());
}

#[test]
fn a_peer_that_may_only_write_cannot_use_a_delta() {
    let fixture = fixture(&[Permission::FsWrite]);
    std::fs::write(fixture.root.join("f"), pseudo(5000, 15)).unwrap();
    let request = WriteRequest {
        mode: WriteMode::Delta {
            block_size: 1024,
            base: DeltaBase {
                size: 5000,
                modified: None,
                hashes: Digests::new(),
            },
        },
        ..WriteRequest::replace("f")
    };
    assert_eq!(
        code(Writer::start(&fixture.link, &request, WAIT)),
        ErrorCode::Denied
    );
    // A plain replace is still allowed.
    assert!(Writer::start(&fixture.link, &WriteRequest::replace("g"), WAIT).is_ok());
}

#[test]
fn a_digest_the_new_file_does_not_have_leaves_the_target_as_it_was() {
    let fixture = fixture(&RW);
    let old = pseudo(50_000, 16);
    let host = fixture.root.join("f");
    std::fs::write(&host, &old).unwrap();
    let seen = hash_blocks(
        &fixture.link,
        HashItem::whole("f"),
        &[Algorithm::Blake3],
        &BlockOptions {
            size: Some(2048),
            ..BlockOptions::default()
        },
    )
    .unwrap();
    let request = WriteRequest {
        mode: WriteMode::Delta {
            block_size: 2048,
            base: DeltaBase {
                size: seen.result.size,
                modified: seen.result.modified,
                hashes: Digests::new(),
            },
        },
        expect: Digests::from([(Algorithm::Sha256, "00".repeat(32))]),
        ..WriteRequest::replace("f")
    };
    let writer = Writer::start(&fixture.link, &request, WAIT).unwrap();
    writer.copy(0, 5).unwrap();
    writer.write(b"extra").unwrap();
    assert_eq!(code(writer.finish(WAIT)), ErrorCode::Io);
    assert_eq!(std::fs::read(&host).unwrap(), old);
    assert!(strays(&fixture.root, &["f"]).is_empty());
}

#[test]
fn a_signature_past_the_host_s_bound_is_busy_and_a_block_past_max_block_is_refused() {
    let fixture = fixture_with(
        &RW,
        DeltaConfig {
            max_signature_bytes: 100,
            ..DeltaConfig::default()
        },
    );
    let data = pseudo(1 << 20, 17);
    std::fs::write(fixture.root.join("f"), &data).unwrap();
    std::fs::write(fixture.local.join("f"), pseudo(1 << 20, 18)).unwrap();
    let options = SyncOptions {
        block_size: Some(2048),
        ..always()
    };
    assert_eq!(
        code(get(&fixture.link, "f", &fixture.local.join("f"), &options)),
        ErrorCode::Busy
    );
    // The local file is as it was.
    assert_ne!(std::fs::read(fixture.local.join("f")).unwrap(), data);
    assert!(strays(&fixture.local, &["f"]).is_empty());

    let tight = fixture_with(
        &RW,
        DeltaConfig {
            max_block: 4096,
            ..DeltaConfig::default()
        },
    );
    std::fs::write(tight.root.join("f"), &data).unwrap();
    std::fs::write(tight.local.join("f"), &data).unwrap();
    let wide = SyncOptions {
        block_size: Some(65_536),
        ..always()
    };
    assert_eq!(
        code(get(&tight.link, "f", &tight.local.join("f"), &wide)),
        ErrorCode::BadRequest
    );
    let blocks = hash_blocks(
        &tight.link,
        HashItem::whole("f"),
        &[Algorithm::Blake3],
        &BlockOptions {
            size: Some(65_536),
            ..BlockOptions::default()
        },
    );
    assert_eq!(code(blocks), ErrorCode::BadRequest);
}

#[test]
fn compression_composes_with_a_delta_in_both_directions() {
    let fixture = fixture(&RW);
    // Text compresses, so the literal bytes cost less on the wire than they are.
    let line = b"the quick brown fox jumps over the lazy dog 0123456789\n";
    let old: Vec<u8> = line.iter().cycle().take(2 << 20).copied().collect();
    let mut new = old.clone();
    new.splice(1_000_000..1_000_000, b"INSERTED TEXT ".repeat(2000));
    let compressed = SyncOptions {
        compression: Some(Compression::new(Codec::Zstd).level(3)),
        ..always()
    };
    let (host, mine) = (fixture.root.join("t"), fixture.local.join("t"));
    std::fs::write(&host, &old).unwrap();
    std::fs::write(&mine, &new).unwrap();
    let pushed = put(&fixture.link, &mine, "t", &compressed).unwrap();
    assert!(pushed.delta);
    assert_eq!(std::fs::read(&host).unwrap(), new);
    assert!(
        pushed.wire_bytes < pushed.literal_bytes / 2 + 20_000,
        "{pushed:?}"
    );

    std::fs::write(&mine, &old).unwrap();
    let pulled = get(&fixture.link, "t", &mine, &compressed).unwrap();
    assert!(pulled.delta);
    assert_eq!(std::fs::read(&mine).unwrap(), new);
    assert!(
        pulled.wire_bytes < pulled.literal_bytes / 2 + 20_000,
        "{pulled:?}"
    );
}

#[test]
fn a_delta_read_with_a_digest_reports_the_new_file_s() {
    let fixture = fixture(&READ);
    let new = pseudo(100_000, 19);
    std::fs::write(fixture.root.join("f"), &new).unwrap();
    // The raw wire: an empty old copy, so the host sends it all as data.
    let request = ReadRequest {
        hashes: vec![Algorithm::Sha256],
        delta: Some(bohio_proto::fs::ReadDelta {
            block_size: 4096,
            weak: "rsync".into(),
            strong: Algorithm::Blake3,
            strong_len: 16,
            base_size: 0,
        }),
        ..ReadRequest::whole("f")
    };
    let channel = fixture
        .link
        .open(protocol::READ, &request.to_metadata().unwrap())
        .unwrap();
    channel
        .send_timeout(
            &bohio_proto::fs::ReadInput::SignatureEnd.encode().unwrap(),
            None,
        )
        .unwrap();
    let mut got = Vec::new();
    let mut header = None;
    let done = loop {
        let frame = channel.receive(WAIT).unwrap().expect("a frame");
        match bohio_proto::fs::ReadOutput::decode(&frame).unwrap() {
            bohio_proto::fs::ReadOutput::Header(found) => header = Some(found),
            bohio_proto::fs::ReadOutput::Data(bytes) => got.extend_from_slice(&bytes),
            bohio_proto::fs::ReadOutput::Done(done) => break done,
            other => panic!("unexpected {other:?}"),
        }
    };
    let header = header.unwrap();
    assert!(header.delta);
    assert_eq!((header.size, header.length), (100_000, 100_000));
    assert_eq!(got, new);
    assert_eq!(done.hashes.get(&Algorithm::Sha256), Some(&sha256(&new)));
}

#[test]
fn the_blocks_of_a_file_are_its_signature_and_a_range_numbers_from_its_start() {
    let fixture = fixture(&READ);
    let data = pseudo(100_000, 20);
    std::fs::write(fixture.root.join("f"), &data).unwrap();
    let blocks = hash_blocks(
        &fixture.link,
        HashItem::whole("f"),
        &[Algorithm::Blake3, Algorithm::Md5],
        &BlockOptions {
            size: Some(4096),
            strong_len: Some(8),
            ..BlockOptions::default()
        },
    )
    .unwrap();
    assert_eq!(blocks.signature.blocks(), 25);
    assert_eq!(blocks.signature.spec().strong_len, 8);
    assert_eq!(blocks.result.block_size, Some(4096));
    assert!(blocks.result.hashes.contains_key(&Algorithm::Md5));
    // The host's choice from the size: about the square root as a power of two, at least min_block.
    let chosen = hash_blocks(
        &fixture.link,
        HashItem::whole("f"),
        &[Algorithm::Blake3],
        &BlockOptions::default(),
    )
    .unwrap();
    assert_eq!(chosen.signature.spec().block_size, 2048);
    assert_eq!(chosen.signature.spec().strong_len, 16);
    // A range: blocks of the range, from its start.
    let item = HashItem {
        offset: 4096,
        length: Some(8192),
        ..HashItem::whole("f")
    };
    let range = hash_blocks(
        &fixture.link,
        item,
        &[Algorithm::Blake3],
        &BlockOptions {
            size: Some(4096),
            ..BlockOptions::default()
        },
    )
    .unwrap();
    assert_eq!(range.signature.blocks(), 2);
    // Two blocks of the whole file's signature are its second and third.
    let mut whole_blocks = bohio_transfer::signature::SignatureBuilder::new(
        bohio_transfer::signature::SignatureSpec::new(4096, Algorithm::Blake3, 16).unwrap(),
    )
    .unwrap();
    let mut mine = bohio_transfer::signature::Signature::new(
        bohio_transfer::signature::SignatureSpec::new(4096, Algorithm::Blake3, 16).unwrap(),
        8192,
    )
    .unwrap();
    for block in whole_blocks.update(&data[4096..12_288]) {
        mine.push(&block).unwrap();
    }
    assert_eq!(range.signature, mine);
    // Options the host cannot use.
    assert_eq!(
        code(hash_blocks(
            &fixture.link,
            HashItem::whole("f"),
            &[Algorithm::Crc32c],
            &BlockOptions {
                strong_len: Some(9),
                ..BlockOptions::default()
            },
        )),
        ErrorCode::BadRequest
    );
    assert_eq!(
        code(hash_blocks(
            &fixture.link,
            HashItem::whole("f"),
            &[Algorithm::Blake3],
            &BlockOptions {
                size: Some(0),
                ..BlockOptions::default()
            },
        )),
        ErrorCode::BadRequest
    );
}

#[test]
fn compare_goes_from_size_and_time_to_a_digest_to_blocks() {
    let fixture = fixture(&RW);
    let data = pseudo(500_000, 21);
    let (host, mine) = (fixture.root.join("f"), fixture.local.join("f"));
    assert_eq!(
        compare_file(&fixture.link, &mine, "f", Compare::Meta).unwrap(),
        Verdict::LocalMissing
    );
    std::fs::write(&mine, &data).unwrap();
    assert_eq!(
        compare_file(&fixture.link, &mine, "f", Compare::Meta).unwrap(),
        Verdict::RemoteMissing
    );
    std::fs::write(&host, &data).unwrap();
    // The same bytes, written at different times: a time check says they differ, a digest or the blocks say they do not.
    std::thread::sleep(std::time::Duration::from_millis(30));
    let touched = std::fs::File::options().write(true).open(&mine).unwrap();
    touched.set_modified(std::time::SystemTime::now()).unwrap();
    drop(touched);
    assert_eq!(
        compare_file(&fixture.link, &mine, "f", Compare::Meta).unwrap(),
        Verdict::Differs
    );
    for how in [Compare::Hash(Algorithm::Blake3), Compare::Blocks] {
        assert_eq!(
            compare_file(&fixture.link, &mine, "f", how.clone()).unwrap(),
            Verdict::Same,
            "{how:?}"
        );
    }
    // The same time is the same under a time check.
    let stamp = std::fs::metadata(&host).unwrap().modified().unwrap();
    let again = std::fs::File::options().write(true).open(&mine).unwrap();
    again.set_modified(stamp).unwrap();
    drop(again);
    assert_eq!(
        compare_file(&fixture.link, &mine, "f", Compare::Meta).unwrap(),
        Verdict::Same
    );
    // One byte differs, the size and the time do not.
    let mut other = data.clone();
    other[250_000] ^= 1;
    std::fs::write(&mine, &other).unwrap();
    let again = std::fs::File::options().write(true).open(&mine).unwrap();
    again.set_modified(stamp).unwrap();
    drop(again);
    assert_eq!(
        compare_file(&fixture.link, &mine, "f", Compare::Meta).unwrap(),
        Verdict::Same
    );
    for how in [Compare::Hash(Algorithm::Sha256), Compare::Blocks] {
        assert_eq!(
            compare_file(&fixture.link, &mine, "f", how.clone()).unwrap(),
            Verdict::Differs,
            "{how:?}"
        );
    }
    // A different size is different at every depth, and an unoffered algorithm is the host's refusal.
    std::fs::write(&mine, &data[..1000]).unwrap();
    assert_eq!(
        compare_file(&fixture.link, &mine, "f", Compare::Meta).unwrap(),
        Verdict::Differs
    );
    std::fs::write(&mine, &data).unwrap();
    assert_eq!(
        code(compare_file(
            &fixture.link,
            &mine,
            "f",
            Compare::Hash(Algorithm::Other("nope".into()))
        )),
        ErrorCode::Unsupported
    );
}

#[test]
fn a_file_the_compare_finds_the_same_is_not_moved() {
    let fixture = fixture(&RW);
    let data = pseudo(100_000, 22);
    std::fs::write(fixture.root.join("f"), &data).unwrap();
    std::fs::write(fixture.local.join("f"), &data).unwrap();
    let options = PutOptions {
        compare: Some(Compare::Hash(Algorithm::Blake3)),
        ..SyncOptions::default()
    };
    let moved = put(&fixture.link, &fixture.local.join("f"), "f", &options).unwrap();
    assert!(moved.skipped);
    assert_eq!(moved.wire_bytes, 0);
    let moved = get(&fixture.link, "f", &fixture.local.join("f"), &options).unwrap();
    assert!(moved.skipped);
    // A difference is moved.
    std::fs::write(fixture.local.join("f"), pseudo(100_000, 25)).unwrap();
    let moved = get(&fixture.link, "f", &fixture.local.join("f"), &options).unwrap();
    assert!(!moved.skipped);
    assert_eq!(std::fs::read(fixture.local.join("f")).unwrap(), data);
}

#[test]
fn a_whole_read_still_works_beside_the_delta_ones() {
    // The plain read of a file the delta tests wrote: no delta field, no signature.
    let fixture = fixture(&READ);
    std::fs::write(fixture.root.join("f"), b"hello").unwrap();
    let reader = Reader::start(&fixture.link, &ReadRequest::whole("f"), WAIT).unwrap();
    assert!(!reader.header().delta);
    assert_eq!(reader.read_to_end(WAIT).unwrap(), b"hello");
}
