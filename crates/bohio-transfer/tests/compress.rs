//! The codecs: each algorithm round-trips, the pieces decode as they arrive and cut anywhere, a raw frame
//! ends the context, the formats are the standard ones, and the bounds hold against a hostile stream.

use std::io::Read;

use bohio_proto::compress::{Codec, Compression};
use bohio_transfer::compress::{
    Bounds, Caps, DecodeError, Decoder, Encoder, FREE_OUTPUT, Piece, Unsupported, available,
    compress_all, ratio,
};

fn algorithms() -> Vec<Compression> {
    let mut all = vec![
        Compression::new(Codec::Lz4),
        Compression::new(Codec::Deflate),
        Compression::new(Codec::Deflate).level(1),
        Compression::new(Codec::Deflate).level(9),
    ];
    if cfg!(feature = "zstd") {
        all.push(Compression::new(Codec::Zstd));
        all.push(Compression::new(Codec::Zstd).level(1));
        all.push(Compression::new(Codec::Zstd).level(19));
        all.push(Compression::new(Codec::Zstd).level(3).window_log(24));
    }
    all
}

/// Text that repeats with variation, so every codec finds something and none finds everything.
fn text(length: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(length);
    let mut n = 0_u32;
    while out.len() < length {
        out.extend_from_slice(
            format!(
                "line {n}: the quick brown fox {} jumps over the lazy dog {}\n",
                n % 17,
                n.wrapping_mul(2_654_435_761) % 1000
            )
            .as_bytes(),
        );
        n += 1;
    }
    out.truncate(length);
    out
}

/// Bytes a codec cannot shrink: a xorshift stream.
fn random(length: usize) -> Vec<u8> {
    let mut state = 0x2545_F491_4F6C_DD1D_u64;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

fn decode_pieces(request: &Compression, pieces: &[Vec<u8>]) -> Vec<u8> {
    let mut decoder = Decoder::new(request, &Bounds::default()).unwrap();
    let mut out = Vec::new();
    for piece in pieces {
        decoder.feed(piece, &mut out).unwrap();
    }
    out
}

#[test]
fn every_algorithm_round_trips_piece_by_piece() {
    let data = text(1_000_000);
    for request in algorithms() {
        let mut encoder = Encoder::new(&request, &Caps::default()).unwrap();
        let mut pieces = Vec::new();
        for chunk in data.chunks(65_536) {
            let piece = encoder.piece(chunk, true).unwrap();
            // Flushed: this piece alone, with the ones before it, decodes to the chunks so far.
            assert!(!piece.is_empty(), "{request:?} held a flushed chunk back");
            pieces.push(piece);
        }
        pieces.push(encoder.finish().unwrap());
        let compressed: usize = pieces.iter().map(Vec::len).sum();
        assert!(
            compressed < data.len() / 2,
            "{request:?} did not compress text"
        );
        assert_eq!(decode_pieces(&request, &pieces), data, "{request:?}");
    }
}

#[test]
fn each_flushed_piece_decodes_completely_before_the_next_one_exists() {
    let data = text(300_000);
    for request in algorithms() {
        let mut encoder = Encoder::new(&request, &Caps::default()).unwrap();
        let mut decoder = Decoder::new(&request, &Bounds::default()).unwrap();
        let mut seen = Vec::new();
        for chunk in data.chunks(10_000) {
            let piece = encoder.piece(chunk, true).unwrap();
            decoder.feed(&piece, &mut seen).unwrap();
            assert!(seen.len() % 10_000 == 0 || seen.len() == data.len());
        }
        assert_eq!(seen, data, "{request:?}");
    }
}

#[test]
fn a_stream_cut_anywhere_decodes() {
    let data = text(200_000);
    for request in algorithms() {
        let whole = compress_all(&request, &Caps::default(), &data).unwrap();
        for cut in [1_usize, 2, 3, 7, 1000, 65_536] {
            let mut decoder = Decoder::new(&request, &Bounds::default()).unwrap();
            let mut out = Vec::new();
            for part in whole.chunks(cut) {
                decoder.feed(part, &mut out).unwrap();
            }
            assert_eq!(out, data, "{request:?} cut every {cut}");
        }
    }
}

#[test]
fn incompressible_pieces_go_raw_and_a_raw_frame_ends_the_context() {
    let text = text(400_000);
    let noise = random(400_000);
    for request in algorithms() {
        let mut encoder = Encoder::new(&request, &Caps::default()).unwrap();
        let mut decoder = Decoder::new(&request, &Bounds::default()).unwrap();
        let mut out = Vec::new();
        let mut expected = Vec::new();
        let (mut raws, mut compressed) = (0, 0);
        // Text, noise, text again: the stream switches per frame.
        for chunk in text
            .chunks(65_536)
            .chain(noise.chunks(65_536))
            .chain(text.chunks(65_536))
        {
            expected.extend_from_slice(chunk);
            match encoder.frame(chunk).unwrap() {
                Piece::Compressed(piece) => {
                    compressed += 1;
                    decoder.feed(&piece, &mut out).unwrap();
                }
                Piece::Raw => {
                    raws += 1;
                    decoder.reset();
                    out.extend_from_slice(chunk);
                }
            }
        }
        assert!(raws >= 6, "{request:?}: noise was compressed ({raws} raw)");
        assert!(compressed >= 12, "{request:?}: text was not ({compressed})");
        assert_eq!(out, expected, "{request:?}");
    }
}

#[test]
fn a_context_that_continues_after_raw_without_a_reset_would_not_decode() {
    // The rule is load-bearing: the encoder restarts after raw, so a decoder that does not reset
    // sees a new frame where it expects the old one's next block. zstd and zlib refuse it.
    let text = text(200_000);
    let noise = random(65_536);
    let mut stateful = vec![Compression::new(Codec::Deflate)];
    if cfg!(feature = "zstd") {
        stateful.push(Compression::new(Codec::Zstd));
    }
    for request in stateful {
        let mut encoder = Encoder::new(&request, &Caps::default()).unwrap();
        let mut decoder = Decoder::new(&request, &Bounds::default()).unwrap();
        let mut out = Vec::new();
        let Piece::Compressed(first) = encoder.frame(&text[..65_536]).unwrap() else {
            panic!("text compresses");
        };
        decoder.feed(&first, &mut out).unwrap();
        assert!(matches!(encoder.frame(&noise).unwrap(), Piece::Raw));
        let Piece::Compressed(next) = encoder.frame(&text[65_536..131_072]).unwrap() else {
            panic!("text compresses");
        };
        // Skipping `reset` is the mistake: a decoder in the middle of an old stream gets a new one.
        let mut stale = Vec::new();
        assert!(
            decoder.feed(&next, &mut stale).is_err() || stale != text[65_536..131_072],
            "{request:?}"
        );
    }
}

#[test]
fn the_formats_are_the_standard_ones() {
    let data = text(300_000);
    // lz4: the reference frame decoder reads ours, and ours reads the reference encoder's.
    let ours = compress_all(&Compression::new(Codec::Lz4), &Caps::default(), &data).unwrap();
    let mut read = Vec::new();
    lz4_flex::frame::FrameDecoder::new(&ours[..])
        .read_to_end(&mut read)
        .unwrap();
    assert_eq!(read, data);
    let mut theirs = Vec::new();
    {
        use std::io::Write;
        let mut encoder = lz4_flex::frame::FrameEncoder::new(&mut theirs);
        encoder.write_all(&data).unwrap();
        encoder.finish().unwrap();
    }
    let mut decoder = Decoder::new(&Compression::new(Codec::Lz4), &Bounds::default()).unwrap();
    let mut out = Vec::new();
    for part in theirs.chunks(5000) {
        decoder.feed(part, &mut out).unwrap();
    }
    assert_eq!(out, data);

    // deflate: a zlib stream.
    let ours = compress_all(&Compression::new(Codec::Deflate), &Caps::default(), &data).unwrap();
    let mut read = Vec::new();
    flate2::read::ZlibDecoder::new(&ours[..])
        .read_to_end(&mut read)
        .unwrap();
    assert_eq!(read, data);

    // zstd: one frame.
    #[cfg(feature = "zstd")]
    {
        let ours = compress_all(&Compression::new(Codec::Zstd), &Caps::default(), &data).unwrap();
        assert_eq!(zstd::decode_all(&ours[..]).unwrap(), data);
        let theirs = zstd::encode_all(&data[..], 3).unwrap();
        let mut decoder = Decoder::new(&Compression::new(Codec::Zstd), &Bounds::default()).unwrap();
        let mut out = Vec::new();
        decoder.feed(&theirs, &mut out).unwrap();
        assert_eq!(out, data);
    }
}

#[test]
fn levels_are_clamped_to_the_caps_and_reported() {
    let caps = Caps {
        max_level_zstd: 5,
        max_level_deflate: 4,
        window_log_max: 20,
        workers: 0,
    };
    let deflate = caps
        .clamp(&Compression::new(Codec::Deflate).level(9))
        .unwrap();
    assert_eq!(deflate.level, Some(4));
    let lz4 = caps.clamp(&Compression::new(Codec::Lz4).level(9)).unwrap();
    assert_eq!(lz4.level, None);
    assert_eq!(
        caps.clamp(&Compression::new(Codec::Deflate)).unwrap().level,
        Some(4),
        "the default level is capped too"
    );
    #[cfg(feature = "zstd")]
    {
        let zstd = caps
            .clamp(&Compression::new(Codec::Zstd).level(19).window_log(27))
            .unwrap();
        assert_eq!((zstd.level, zstd.window_log), (Some(5), Some(20)));
        assert_eq!(
            caps.clamp(&Compression::new(Codec::Zstd)).unwrap().level,
            Some(3)
        );
        let info = caps.describe(&Codec::Zstd).unwrap();
        assert_eq!((info.levels.max, info.window_log_max), (5, 20));
        assert!(info.levels.min < 0, "zstd has fast negative levels");
    }
    for refused in [Codec::None, Codec::Auto, Codec::Other("brotli".into())] {
        assert_eq!(
            caps.clamp(&Compression::new(refused.clone())),
            Err(Unsupported(refused))
        );
    }
}

#[test]
fn a_higher_level_compresses_better() {
    #[cfg(feature = "zstd")]
    {
        let data = text(2_000_000);
        let low = compress_all(
            &Compression::new(Codec::Zstd).level(1),
            &Caps::default(),
            &data,
        )
        .unwrap()
        .len();
        let high = compress_all(
            &Compression::new(Codec::Zstd).level(19),
            &Caps::default(),
            &data,
        )
        .unwrap()
        .len();
        assert!(high < low, "level 19 {high} against level 1 {low}");
    }
    let data = text(500_000);
    let low = compress_all(
        &Compression::new(Codec::Deflate).level(1),
        &Caps::default(),
        &data,
    )
    .unwrap()
    .len();
    let high = compress_all(
        &Compression::new(Codec::Deflate).level(9),
        &Caps::default(),
        &data,
    )
    .unwrap()
    .len();
    assert!(high <= low);
}

#[test]
fn sampling_tells_text_from_noise() {
    for request in algorithms() {
        let caps = Caps::default();
        let on_text = ratio(&request, &caps, &text(65_536)).unwrap();
        let on_noise = ratio(&request, &caps, &random(65_536)).unwrap();
        assert!(on_text < 0.5, "{request:?} text {on_text}");
        assert!(on_noise > 0.9, "{request:?} noise {on_noise}");
    }
}

#[test]
fn a_frame_asking_for_a_window_over_the_bound_is_refused() {
    #[cfg(feature = "zstd")]
    {
        let data = text(100_000);
        let request = Compression::new(Codec::Zstd).window_log(27);
        let caps = Caps {
            window_log_max: 27,
            ..Caps::default()
        };
        let frame = compress_all(&request, &caps, &data).unwrap();
        let strict = Bounds {
            window_log_max: 17,
            ..Bounds::default()
        };
        let mut decoder = Decoder::new(&request, &strict).unwrap();
        let mut out = Vec::new();
        assert!(matches!(
            decoder.feed(&frame, &mut out),
            Err(DecodeError::Corrupt)
        ));
        assert!(
            out.is_empty(),
            "nothing was produced under a refused window"
        );
        // The same frame under a bound that allows it.
        let mut decoder = Decoder::new(&request, &Bounds::default()).unwrap();
        decoder.feed(&frame, &mut out).unwrap();
        assert_eq!(out, data);
    }
}

#[test]
fn output_past_the_announced_length_stops_the_decoder() {
    let data = text(300_000);
    for request in algorithms() {
        let whole = compress_all(&request, &Caps::default(), &data).unwrap();
        let bounds = Bounds {
            max_output: Some(100_000),
            ..Bounds::default()
        };
        let mut decoder = Decoder::new(&request, &bounds).unwrap();
        let mut out = Vec::new();
        let result = decoder.feed(&whole, &mut out);
        assert!(matches!(result, Err(DecodeError::TooLong)), "{request:?}");
        assert!(out.len() <= 100_000, "{request:?} wrote {}", out.len());
    }
}

#[test]
fn expansion_is_held_after_the_first_mebibyte() {
    let zeros = vec![0_u8; 16 * 1024 * 1024];
    for request in algorithms() {
        let caps = Caps::default();
        let mut encoder = Encoder::new(&request, &caps).unwrap();
        let mut pieces = Vec::new();
        for chunk in zeros.chunks(65_536) {
            pieces.push(encoder.piece(chunk, true).unwrap());
        }
        let held = Bounds {
            max_expansion: 50,
            ..Bounds::default()
        };
        let mut decoder = Decoder::new(&request, &held).unwrap();
        let mut out = Vec::new();
        let mut result = Ok(0);
        for piece in &pieces {
            result = decoder.feed(piece, &mut out);
            if result.is_err() {
                break;
            }
        }
        assert!(
            matches!(result, Err(DecodeError::Expansion)),
            "{request:?}: {result:?}"
        );
        assert!(
            (out.len() as u64) < 4 * FREE_OUTPUT,
            "{request:?} let {} bytes through",
            out.len()
        );
        let mut decoder = Decoder::new(&request, &Bounds::default()).unwrap();
        let mut out = Vec::new();
        for piece in &pieces {
            decoder.feed(piece, &mut out).unwrap();
        }
        assert_eq!(out.len(), zeros.len(), "{request:?}");
    }
}

#[test]
fn expansion_survives_a_reset() {
    // Alternating raw and compressed frames must not restart the count.
    let request = Compression::new(Codec::Deflate);
    let zeros = vec![0_u8; 65_536];
    let bounds = Bounds {
        max_expansion: 10,
        ..Bounds::default()
    };
    let mut encoder = Encoder::new(&request, &Caps::default()).unwrap();
    let mut decoder = Decoder::new(&request, &bounds).unwrap();
    let mut out = Vec::new();
    let mut failed = false;
    for _ in 0..64 {
        let piece = encoder.piece(&zeros, true).unwrap();
        if decoder.feed(&piece, &mut out).is_err() {
            failed = true;
            break;
        }
        decoder.reset();
        encoder.restart();
    }
    assert!(failed, "64 resets hid {} bytes of expansion", out.len());
}

#[test]
fn garbage_is_refused_not_crashed_on() {
    for request in algorithms() {
        let mut decoder = Decoder::new(&request, &Bounds::default()).unwrap();
        let mut out = Vec::new();
        let result = decoder.feed(&random(5000), &mut out);
        assert!(result.is_err(), "{request:?} accepted noise");
    }
}

#[test]
fn what_this_build_offers_and_refuses() {
    let offered = available();
    assert!(offered.contains(&Codec::Lz4) && offered.contains(&Codec::Deflate));
    assert_eq!(offered.contains(&Codec::Zstd), cfg!(feature = "zstd"));
    if !cfg!(feature = "zstd") {
        assert!(matches!(
            Encoder::new(&Compression::new(Codec::Zstd), &Caps::default()),
            Err(Unsupported(Codec::Zstd))
        ));
        assert!(matches!(
            Decoder::new(&Compression::new(Codec::Zstd), &Bounds::default()),
            Err(DecodeError::Unsupported(_))
        ));
    }
    for codec in [Codec::None, Codec::Auto] {
        assert!(Encoder::new(&Compression::new(codec), &Caps::default()).is_err());
    }
}

#[test]
fn compression_text_parses() {
    assert_eq!(
        Compression::parse("zstd:19").unwrap(),
        Compression::new(Codec::Zstd).level(19)
    );
    assert_eq!(
        Compression::parse("lz4").unwrap(),
        Compression::new(Codec::Lz4)
    );
    assert_eq!(
        Compression::parse("zstd:-5").unwrap(),
        Compression::new(Codec::Zstd).level(-5)
    );
    assert!(Compression::parse("zstd:fast").is_err());
}

#[test]
fn the_pieces_in_the_vectors_decode() {
    // The same bytes `vectors/read.json` and the others hold as compressed frames.
    let wanted = b"hello hello hello hello hello hello\n";
    let pieces = [
        (Codec::Zstd, "28b52ffd00586c00003868656c6c6f200a0100b94b11"),
        (
            Codec::Lz4,
            "04224d18607073110000006f68656c6c6f200600056068656c6c6f0a",
        ),
        (Codec::Deflate, "789cca48cdc9c957c8c0477201000000ffff"),
    ];
    for (codec, hex) in pieces {
        if codec == Codec::Zstd && !cfg!(feature = "zstd") {
            continue;
        }
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).unwrap())
            .collect();
        let mut decoder =
            Decoder::new(&Compression::new(codec.clone()), &Bounds::default()).unwrap();
        let mut out = Vec::new();
        decoder.feed(&bytes, &mut out).unwrap();
        assert_eq!(out, wanted, "{codec}");
    }
}
