//! The delta algorithm as data: `vectors/delta.json` at the repository root holds weak sums, a signature and a
//! stream of steps, and a client in any language reproduces them.
//!
//! The weak sums are written from the definition (`a = sum(x[i])`, `b = sum((L - i) * x[i])`, both mod 2^16, value
//! `a + 2^16 * b`), never from the rolling code; the test then checks the rolling code against them. As in the
//! wire's vectors, a clean tree after `cargo test` means the file is current.

use std::io::Cursor;
use std::path::Path;

use bohio_proto::hash::Algorithm;
use bohio_transfer::apply::apply;
use bohio_transfer::delta::{Matcher, Op};
use bohio_transfer::rolling::Rolling;
use bohio_transfer::signature::{Signature, SignatureBuilder, SignatureSpec};
use serde_json::{Value, json};

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The weak sum from its definition: `(a, b, value)`.
fn definition(window: &[u8]) -> (u64, u64, u64) {
    let length = window.len() as u64;
    let a = window.iter().map(|byte| u64::from(*byte)).sum::<u64>() % 65_536;
    let b = window
        .iter()
        .enumerate()
        .map(|(at, byte)| (length - at as u64) * u64::from(*byte))
        .sum::<u64>()
        % 65_536;
    (a, b, a + 65_536 * b)
}

/// The generated input of the rolled vector: byte `i` is `(i * i / 7 + 5 * i + 11) mod 256`.
fn generated(length: u64) -> Vec<u8> {
    (0..length)
        .map(|i| ((i * i / 7 + 5 * i + 11) % 256) as u8)
        .collect()
}

fn weak_vector(name: &str, input: &[u8]) -> Value {
    let (a, b, value) = definition(input);
    assert_eq!(u64::from(Rolling::new(input).value()), value, "{name}");
    json!({
        "name": name,
        "input_hex": hex(input),
        "a": a,
        "b": b,
        "weak": format!("{value:08x}"),
    })
}

fn document() -> Value {
    let window = generated(65_536 + 1);
    let (before, after) = (&window[..65_536], &window[1..]);
    let (_, _, rolled_before) = definition(before);
    let (_, _, rolled_after) = definition(after);
    let mut rolling = Rolling::new(before);
    assert_eq!(u64::from(rolling.value()), rolled_before);
    rolling.roll(window[0], window[65_536]);
    assert_eq!(u64::from(rolling.value()), rolled_after);

    // A signature: 36 bytes in blocks of 8, the strong hash blake3 cut to 4 bytes.
    let base = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let spec = SignatureSpec::new(8, Algorithm::Blake3, 4).unwrap();
    let mut builder = SignatureBuilder::new(spec.clone()).unwrap();
    let mut signature = Signature::new(spec.clone(), base.len() as u64).unwrap();
    let mut blocks = Vec::new();
    for block in builder.update(base).into_iter().chain(builder.finish()) {
        blocks.push(
            json!({ "weak": format!("{:08x}", block.weak), "strong_hex": hex(&block.strong) }),
        );
        signature.push(&block).unwrap();
    }

    // A new file made of blocks 0 and 1 (in order), twelve new bytes, block 3 and the short last block.
    let mut new = base[..16].to_vec();
    new.extend_from_slice(b"NEW-12-BYTES");
    new.extend_from_slice(&base[24..32]);
    new.extend_from_slice(&base[32..]);
    let mut matcher = Matcher::new(signature, 5).unwrap();
    let mut ops = matcher.feed(&new);
    ops.extend(matcher.finish());
    let mut rebuilt = Vec::new();
    let mut source = Cursor::new(base.to_vec());
    let steps: Vec<Value> = ops
        .iter()
        .map(|op| {
            apply(&mut source, op, 8, &mut rebuilt).unwrap();
            match op {
                Op::Copy { block, count } => json!({ "copy": { "block": block, "count": count } }),
                Op::Literal(bytes) => json!({ "literal_hex": hex(bytes) }),
            }
        })
        .collect();
    assert_eq!(rebuilt, new);

    json!({
        "protocol": "rsync delta of bohio-read/1 and bohio-write/1",
        "encoding": "weak is the 32-bit value in 8 hex digits, the checksum a + 65536 * b of docs/protocol.md \"Deltas\"; a vector's input is hex",
        "weak_vectors": [
            weak_vector("empty", b""),
            weak_vector("abc", b"abc"),
            weak_vector("one_byte", &[0xff]),
            weak_vector("wraps_at_16_bits", &vec![0xff_u8; 300]),
        ],
        "rolled": {
            "name": "a 64 KiB window rolled one byte",
            "generator": "byte i is (i * i / 7 + 5 * i + 11) mod 256, for i from 0 to 65536 inclusive",
            "window": 65_536,
            "weak_at_0": format!("{rolled_before:08x}"),
            "weak_at_1": format!("{rolled_after:08x}"),
        },
        "signature": {
            "name": "blocks of 8, blake3 cut to 4 bytes",
            "base_hex": hex(base),
            "block_size": 8,
            "strong": "blake3",
            "strong_len": 4,
            "blocks": blocks,
        },
        "delta": {
            "name": "the signature above against a new file, literals cut at 5 bytes",
            "new_hex": hex(&new),
            "literal_frame": 5,
            "note": "steps are not unique: a receiver applies any steps that rebuild the new file; these are what the Rust matcher emits",
            "steps": steps,
        },
    })
}

#[test]
fn the_delta_vectors_are_current_and_the_weak_sums_follow_the_definition() {
    let rendered = {
        let mut text = serde_json::to_string_pretty(&document()).unwrap();
        text.push('\n');
        text
    };
    // A packaged crate has no repository around it: the vectors are checked where they exist.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    if root.join("Cargo.toml").exists() {
        let path = root.join("vectors").join("delta.json");
        if std::fs::read_to_string(&path).ok().as_deref() != Some(rendered.as_str()) {
            std::fs::write(&path, rendered.as_bytes()).unwrap();
        }
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // The three weak sums the file must hold: an empty block, `abc` and a window of 64 KiB rolled one byte.
        let weak = on_disk["weak_vectors"].as_array().unwrap();
        assert_eq!(weak[0]["weak"], "00000000");
        assert_eq!(weak[1]["weak"], "024a0126");
        assert_ne!(
            on_disk["rolled"]["weak_at_0"],
            on_disk["rolled"]["weak_at_1"]
        );
    }
}
