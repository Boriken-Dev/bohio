//! Each algorithm against the published test vectors of its specification or reference tool: the empty
//! input, `abc` and, for CRC-32C, the catalogue's `123456789`.

use bohio_proto::hash::Algorithm;
use bohio_transfer::hash::{Hasher, Unsupported, available, digest_reader};

fn digest(algorithm: &Algorithm, input: &[u8]) -> String {
    let mut hasher = Hasher::new(std::slice::from_ref(algorithm)).unwrap();
    hasher.update(input);
    hasher.finish().remove(algorithm).unwrap()
}

/// `(algorithm, digest of the empty input, digest of "abc")`.
fn published() -> Vec<(Algorithm, &'static str, &'static str)> {
    vec![
        (
            Algorithm::Md5,
            "d41d8cd98f00b204e9800998ecf8427e",
            "900150983cd24fb0d6963f7d28e17f72",
        ),
        (
            Algorithm::Sha1,
            "da39a3ee5e6b4b0d3255bfef95601890afd80709",
            "a9993e364706816aba3e25717850c26c9cd0d89d",
        ),
        (
            Algorithm::Sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ),
        (
            Algorithm::Sha384,
            "38b060a751ac96384cd9327eb1b1e36a21fdb71114be07434c0cc7bf63f6e1da274edebfe76f65fbd51ad2f14898b95b",
            "cb00753f45a35e8bb5a03d699ac65007272c32ab0eded1631a8b605a43ff5bed8086072ba1e7cc2358baeca134c825a7",
        ),
        (
            Algorithm::Sha512,
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e",
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f",
        ),
        (
            Algorithm::Blake3,
            "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262",
            "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85",
        ),
        (Algorithm::Xxh3, "2d06800538d394c2", "78af5f94892f3950"),
        (
            Algorithm::Xxh128,
            "99aa06d3014798d86001c324468d497f",
            "06b05ab6733a618578af5f94892f3950",
        ),
        (Algorithm::Xxh64, "ef46db3751d8e999", "44bc2cf5ad770999"),
        (Algorithm::Crc32c, "00000000", "364b3fb7"),
    ]
}

#[test]
fn every_algorithm_matches_its_published_vectors_for_the_empty_input_and_abc() {
    let table = published();
    assert_eq!(table.len(), Algorithm::KNOWN.len());
    for (algorithm, empty, abc) in table {
        assert_eq!(
            digest(&algorithm, b""),
            empty,
            "{algorithm} of the empty input"
        );
        assert_eq!(digest(&algorithm, b"abc"), abc, "{algorithm} of abc");
    }
}

#[test]
fn crc32c_of_the_catalogue_check_string() {
    assert_eq!(digest(&Algorithm::Crc32c, b"123456789"), "e3069283");
}

#[test]
fn a_digest_has_the_hex_length_its_algorithm_names() {
    for algorithm in available() {
        assert_eq!(
            Some(digest(&algorithm, b"abc").len()),
            algorithm.hex_len(),
            "{algorithm}"
        );
    }
}

#[test]
fn several_algorithms_in_one_pass_equal_each_alone_whatever_the_piece_size() {
    let data: Vec<u8> = (0..100_000_u32).map(|i| (i * 31 % 251) as u8).collect();
    let all = available();
    for piece in [1_usize, 7, 4096, 99_999, 100_000] {
        let mut together = Hasher::new(&all).unwrap();
        for part in data.chunks(piece) {
            together.update(part);
        }
        let together = together.finish();
        for algorithm in &all {
            assert_eq!(
                together[algorithm],
                digest(algorithm, &data),
                "{algorithm} in pieces of {piece}"
            );
        }
    }
}

#[test]
fn an_algorithm_named_twice_is_computed_once() {
    let mut hasher = Hasher::new(&[Algorithm::Sha1, Algorithm::Sha1]).unwrap();
    hasher.update(b"abc");
    assert_eq!(hasher.finish().len(), 1);
}

#[test]
fn an_algorithm_this_build_does_not_know_is_refused_by_name() {
    let unknown = Algorithm::Other("whirlpool".into());
    let error = Hasher::new(&[Algorithm::Md5, unknown.clone()]).unwrap_err();
    assert_eq!(error, Unsupported(unknown));
    assert!(error.to_string().contains("whirlpool"));
}

#[test]
fn a_reader_is_digested_to_its_end_and_counted() {
    let data = vec![b'a'; 10_000];
    let (digests, count) = digest_reader(&mut data.as_slice(), &[Algorithm::Md5], 777).unwrap();
    assert_eq!(count, 10_000);
    assert_eq!(digests[&Algorithm::Md5], digest(&Algorithm::Md5, &data));
}
