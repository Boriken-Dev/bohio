//! Signatures, matching and applying: a delta of any new file against any old file rebuilds the new file.

use std::io::Cursor;

use bohio_proto::hash::Algorithm;
use bohio_transfer::apply::{ApplyError, apply, copy_len};
use bohio_transfer::delta::{Matcher, Op};
use bohio_transfer::rolling::Rolling;
use bohio_transfer::signature::{
    BlockSig, Signature, SignatureBuilder, SignatureSpec, SpecError, block_size_for,
};

/// A deterministic generator: the tests need random-looking bytes and choices that repeat.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % bound.max(1) as u64).unwrap()
    }

    fn random(&mut self, bound: usize) -> Vec<u8> {
        let len = self.below(bound);
        self.bytes(len)
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next().to_le_bytes()[0]).collect()
    }
}

/// The weak checksum straight from its definition: no rolling.
fn direct(window: &[u8]) -> u32 {
    let len = window.len() as u64;
    let a: u64 = window.iter().map(|byte| u64::from(*byte)).sum::<u64>() % 65536;
    let b: u64 = window
        .iter()
        .enumerate()
        .map(|(index, byte)| (len - index as u64) * u64::from(*byte))
        .sum::<u64>()
        % 65536;
    u32::try_from(a + 65536 * b).unwrap()
}

fn spec(block: u32) -> SignatureSpec {
    SignatureSpec::new(block, Algorithm::Blake3, 16).unwrap()
}

fn signature_of(base: &[u8], block: u32, feed: usize) -> Signature {
    let mut builder = SignatureBuilder::new(spec(block)).unwrap();
    let mut signature = Signature::new(spec(block), base.len() as u64).unwrap();
    for piece in base.chunks(feed.max(1)) {
        for sig in builder.update(piece) {
            signature.push(&sig).unwrap();
        }
    }
    if let Some(sig) = builder.finish() {
        signature.push(&sig).unwrap();
    }
    signature
}

fn delta_of(signature: &Signature, new: &[u8], feed: usize, frame: usize) -> Vec<Op> {
    let mut matcher = Matcher::new(signature.clone(), frame).unwrap();
    let mut ops = Vec::new();
    for piece in new.chunks(feed.max(1)) {
        ops.extend(matcher.feed(piece));
    }
    ops.extend(matcher.finish());
    ops
}

fn rebuild(base: &[u8], block: u32, ops: &[Op]) -> Vec<u8> {
    let mut source = Cursor::new(base.to_vec());
    let mut out = Vec::new();
    for op in ops {
        apply(&mut source, op, block, &mut out).unwrap();
    }
    out
}

fn literal_bytes(ops: &[Op]) -> usize {
    ops.iter()
        .map(|op| match op {
            Op::Literal(bytes) => bytes.len(),
            Op::Copy { .. } => 0,
        })
        .sum()
}

#[test]
fn the_weak_sum_of_a_window_is_the_definition_and_rolling_keeps_it() {
    let mut rng = Rng(11);
    assert_eq!(Rolling::new(&[]).value(), 0);
    assert_eq!(Rolling::new(b"abc").value(), direct(b"abc"));
    for case in 0..200 {
        let length = 1 + rng.below(3000);
        let data = rng.bytes(length + 400);
        let mut rolling = Rolling::new(&data[..length]);
        assert_eq!(rolling.value(), direct(&data[..length]), "case {case}");
        for step in 0..400 {
            rolling.roll(data[step], data[step + length]);
            assert_eq!(
                rolling.value(),
                direct(&data[step + 1..step + 1 + length]),
                "case {case} step {step}"
            );
        }
    }
}

#[test]
fn a_block_of_all_ones_bytes_wraps_its_sums_at_sixteen_bits() {
    // 70000 bytes of 0xff: a = 70000 * 255 mod 65536, b larger still.
    let window = vec![0xff_u8; 70_000];
    assert_eq!(Rolling::new(&window).value(), direct(&window));
}

#[test]
fn the_block_size_is_the_square_root_as_a_power_of_two_within_the_bounds() {
    assert_eq!(block_size_for(0, 2048, 1 << 20), 2048);
    assert_eq!(block_size_for(64 << 20, 2048, 1 << 20), 8192);
    assert_eq!(block_size_for(256 << 20, 2048, 1 << 20), 16384);
    assert_eq!(block_size_for(u64::MAX, 2048, 1 << 20), 1 << 20);
    assert_eq!(block_size_for(1 << 40, 2048, 4096), 4096);
    // The minimum wins over a maximum below it.
    assert_eq!(block_size_for(1 << 40, 4096, 2048), 4096);
}

#[test]
fn a_spec_is_checked() {
    assert_eq!(
        SignatureSpec::new(0, Algorithm::Blake3, 16),
        Err(SpecError::BlockSize)
    );
    assert!(matches!(
        SignatureSpec::new(1024, Algorithm::Blake3, 0),
        Err(SpecError::StrongLen(32))
    ));
    assert!(matches!(
        SignatureSpec::new(1024, Algorithm::Crc32c, 5),
        Err(SpecError::StrongLen(4))
    ));
    assert!(matches!(
        SignatureSpec::new(1024, Algorithm::Other("nope".into()), 4),
        Err(SpecError::Algorithm(_))
    ));
    assert!(SignatureSpec::new(1024, Algorithm::Crc32c, 4).is_ok());
}

#[test]
fn a_signature_is_the_same_whatever_size_the_file_is_fed_in() {
    let mut rng = Rng(5);
    let base = rng.bytes(100_003);
    let whole = signature_of(&base, 4096, base.len());
    assert_eq!(whole.blocks(), 25);
    assert!(whole.is_complete());
    for feed in [1, 7, 4095, 4096, 4097, 50_000] {
        assert_eq!(
            signature_of(&base, 4096, feed),
            whole,
            "fed {feed} at a time"
        );
    }
    assert_eq!(whole.last_len(), 100_003 - 24 * 4096);
}

#[test]
fn a_batch_that_does_not_fit_the_signature_is_refused() {
    let mut signature = Signature::new(spec(1024), 2048).unwrap();
    assert_eq!(
        signature.push_batch(&[1, 2], &[0; 5]),
        Err(SpecError::Batch)
    );
    assert_eq!(
        signature.push_batch(&[1, 2, 3], &[0; 48]),
        Err(SpecError::TooMany)
    );
    assert!(matches!(
        Matcher::new(signature.clone(), 100),
        Err(SpecError::Incomplete)
    ));
    signature.push_batch(&[1, 2], &[0; 32]).unwrap();
    assert!(Matcher::new(signature, 100).is_ok());
}

#[test]
fn identical_files_move_one_copy_and_no_bytes() {
    let mut rng = Rng(9);
    let base = rng.bytes(200_000);
    for block in [2048, 4096, 65_536] {
        let signature = signature_of(&base, block, 8192);
        let ops = delta_of(&signature, &base, 10_000, 65_536);
        assert_eq!(literal_bytes(&ops), 0, "block {block}");
        assert_eq!(ops.len(), 1, "block {block}: {ops:?}");
        assert_eq!(rebuild(&base, block, &ops), base);
    }
}

#[test]
fn one_changed_byte_costs_about_a_block_of_literal() {
    let mut rng = Rng(21);
    let base = rng.bytes(4 << 20);
    let mut new = base.clone();
    new[1_234_567] ^= 0x55;
    let block = block_size_for(base.len() as u64, 2048, 1 << 20);
    assert_eq!(block, 2048);
    let signature = signature_of(&base, block, 65_536);
    let ops = delta_of(&signature, &new, 65_536, 65_536);
    assert_eq!(literal_bytes(&ops), 2048, "{} ops", ops.len());
    assert_eq!(rebuild(&base, block, &ops), new);
}

#[test]
fn an_insertion_at_the_start_shifts_every_block_and_they_are_still_found() {
    let mut rng = Rng(33);
    let base = rng.bytes(1 << 20);
    let mut new = rng.bytes(1024);
    new.extend_from_slice(&base);
    let signature = signature_of(&base, 4096, 4096);
    let ops = delta_of(&signature, &new, 30_000, 65_536);
    assert_eq!(literal_bytes(&ops), 1024);
    assert_eq!(rebuild(&base, 4096, &ops), new);
}

#[test]
fn a_truncated_file_an_appended_file_and_an_empty_one_rebuild() {
    let mut rng = Rng(77);
    let base = rng.bytes(50_001);
    let signature = signature_of(&base, 2048, 2048);
    for new in [
        base[..30_000].to_vec(),
        base[..2048 * 3].to_vec(),
        [base.clone(), rng.bytes(5000)].concat(),
        Vec::new(),
        rng.bytes(10),
    ] {
        let ops = delta_of(&signature, &new, 4096, 4096);
        assert_eq!(rebuild(&base, 2048, &ops), new, "new length {}", new.len());
    }
}

#[test]
fn the_short_last_block_of_the_old_file_is_matched_at_the_end() {
    let mut rng = Rng(3);
    let base = rng.bytes(10_000 + 123);
    let signature = signature_of(&base, 1024, 1000);
    let mut new = rng.bytes(500);
    new.extend_from_slice(&base[1024 * 5..]);
    let ops = delta_of(&signature, &new, 700, 65_536);
    assert_eq!(rebuild(&base, 1024, &ops), new);
    assert_eq!(literal_bytes(&ops), 500, "{ops:?}");
}

#[test]
fn an_empty_old_file_gives_one_literal_run_cut_at_the_frame_size() {
    let mut rng = Rng(8);
    let new = rng.bytes(10_000);
    let signature = signature_of(&[], 2048, 100);
    assert_eq!(signature.blocks(), 0);
    let ops = delta_of(&signature, &new, 999, 4096);
    assert!(
        ops.iter()
            .all(|op| matches!(op, Op::Literal(bytes) if bytes.len() <= 4096))
    );
    assert_eq!(rebuild(&[], 2048, &ops), new);
}

#[test]
fn literals_never_pass_the_frame_size_and_copies_are_merged() {
    let mut rng = Rng(14);
    let base = rng.bytes(300_000);
    let mut new = base[..100_000].to_vec();
    new.extend(rng.bytes(20_000));
    new.extend_from_slice(&base[100_000..]);
    let signature = signature_of(&base, 2048, 2048);
    let ops = delta_of(&signature, &new, 1000, 3000);
    let mut previous_copy_end = None;
    for op in &ops {
        match op {
            Op::Literal(bytes) => {
                assert!(bytes.len() <= 3000);
                previous_copy_end = None;
            }
            Op::Copy { block, count } => {
                assert_ne!(
                    previous_copy_end,
                    Some(*block),
                    "two copies that could be one"
                );
                previous_copy_end = Some(block + count);
            }
        }
    }
    assert_eq!(rebuild(&base, 2048, &ops), new);
}

#[test]
fn repeated_blocks_are_copied_in_runs_where_the_old_file_has_them_in_a_row() {
    // The old file has four zero blocks in a row; the new one has eight.
    let mut base = vec![1_u8; 1024];
    base.extend(vec![0_u8; 4096]);
    base.extend(vec![2_u8; 1024]);
    let mut new = vec![0_u8; 8192];
    new.splice(0..0, vec![1_u8; 1024]);
    let signature = signature_of(&base, 1024, 1024);
    let ops = delta_of(&signature, &new, 1024, 65_536);
    assert_eq!(rebuild(&base, 1024, &ops), new);
    assert_eq!(literal_bytes(&ops), 0);
    // The first run is the four in a row, not eight single copies.
    assert!(ops.contains(&Op::Copy { block: 1, count: 4 }), "{ops:?}");
}

#[test]
fn a_copy_past_the_end_of_the_old_file_is_refused() {
    let base = vec![7_u8; 2500];
    let mut source = Cursor::new(base);
    let mut out = Vec::new();
    for (block, count) in [(3, 1), (0, 4), (2, 2), (0, 0), (u64::MAX, 1), (1, u64::MAX)] {
        assert_eq!(
            apply(&mut source, &Op::Copy { block, count }, 1024, &mut out),
            Err(ApplyError::PastEnd),
            "{block} {count}"
        );
    }
    assert!(out.is_empty());
    // The last block is short: three blocks are 2500 bytes.
    assert_eq!(
        apply(
            &mut source,
            &Op::Copy { block: 0, count: 3 },
            1024,
            &mut out
        ),
        Ok(2500)
    );
    assert_eq!(copy_len(2500, 1024, 2, 1), Some(452));
    assert_eq!(copy_len(0, 1024, 0, 1), None);
}

#[test]
fn any_edit_of_any_file_rebuilds_for_block_sizes_from_2k_to_1m() {
    let mut rng = Rng(0xdead_beef);
    for case in 0..160 {
        let block = 2048_u32 << rng.below(10);
        let size = rng.below(if block > 65_536 { 3_000_000 } else { 300_000 });
        let base = rng.bytes(size);
        let mut new = base.clone();
        for _ in 0..rng.below(6) {
            if new.is_empty() {
                new = rng.random(5000);
                continue;
            }
            let at = rng.below(new.len());
            match rng.below(6) {
                0 => new[at] ^= 1 + rng.below(255) as u8,
                1 => {
                    let extra = rng.random(5000);
                    new.splice(at..at, extra);
                }
                2 => {
                    let end = (at + rng.below(5000)).min(new.len());
                    new.drain(at..end);
                }
                3 => new.truncate(at),
                4 => new.extend(rng.random(5000)),
                _ => {
                    let end = (at + rng.below(20_000)).min(new.len());
                    let piece = new[at..end].to_vec();
                    let to = rng.below(new.len());
                    new.splice(to..to, piece);
                }
            }
        }
        let signature = signature_of(&base, block, 1 + rng.below(70_000));
        let frame = 1 + rng.below(70_000);
        let ops = delta_of(&signature, &new, 1 + rng.below(70_000), frame);
        for op in &ops {
            if let Op::Literal(bytes) = op {
                assert!(!bytes.is_empty() && bytes.len() <= frame, "case {case}");
            }
        }
        assert_eq!(
            rebuild(&base, block, &ops),
            new,
            "case {case}: block {block}, base {} bytes, new {} bytes",
            base.len(),
            new.len()
        );
    }
}

#[test]
fn a_signature_of_equal_weak_values_that_never_match_is_matched_in_bounded_time() {
    const BLOCKS: u64 = 1 << 15;
    let spec = SignatureSpec::new(1, Algorithm::Blake3, 1).unwrap();
    let weak = Rolling::new(&[0]).value();
    let wrong = spec.strong_of(&[0])[0].wrapping_add(1);
    let mut signature = Signature::new(spec, BLOCKS).unwrap();
    for _ in 0..BLOCKS {
        signature
            .push(&BlockSig {
                weak,
                strong: vec![wrong],
            })
            .unwrap();
    }
    let mut matcher = Matcher::new(signature, 4096).unwrap();
    let started = std::time::Instant::now();
    let ops = matcher.feed(&vec![0_u8; BLOCKS as usize]);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(3),
        "matching took {:?}",
        started.elapsed()
    );
    assert!(ops.iter().all(|op| matches!(op, Op::Literal(_))));
}

#[test]
fn equal_blocks_of_the_old_file_are_found_and_the_one_that_continues_the_last_copy_is_preferred() {
    let block = vec![7_u8; 8];
    let old: Vec<u8> = block.iter().chain(&block).chain(&block).copied().collect();
    let signature = signature_of(&old, 8, 8);
    let mut matcher = Matcher::new(signature, 64).unwrap();
    let mut ops = matcher.feed(&old);
    ops.extend(matcher.finish());
    assert_eq!(ops, vec![Op::Copy { block: 0, count: 3 }]);
}
