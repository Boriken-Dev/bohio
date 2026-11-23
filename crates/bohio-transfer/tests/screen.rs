//! The screen's tiles, rectangle encodings and compositor.

#![forbid(unsafe_code)]

use bohio_proto::gui::{DisplayId, Rect, RectEncoding, UpdateHeader, UpdateRect};
use bohio_transfer::screen::{
    Compositor, EncodeParams, Limits, Pixels, ScreenError, TileGrid, decode_rect, encode_rect,
};

/// A frame whose pixels are a smooth picture (a JPEG does well) with some hard edges (text does not).
fn picture(width: u32, height: u32, seed: u32) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    for y in 0..height {
        for x in 0..width {
            let edge = if (x / 7 + y / 5 + seed).is_multiple_of(11) {
                120
            } else {
                0
            };
            rgb.push(((x * 3 + seed) % 150) as u8 + edge / 2);
            rgb.push(((y * 2 + seed * 3) % 200) as u8);
            rgb.push((((x + y) / 2 + seed * 5) % 150) as u8 + edge / 3);
        }
    }
    rgb
}

fn set(rgb: &mut [u8], width: u32, x: u32, y: u32, color: [u8; 3]) {
    let at = ((y * width + x) * 3) as usize;
    rgb[at..at + 3].copy_from_slice(&color);
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let sum: f64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| {
            let d = f64::from(*x) - f64::from(*y);
            d * d
        })
        .sum();
    if sum == 0.0 {
        return f64::INFINITY;
    }
    10.0 * (255.0_f64 * 255.0 / (sum / a.len() as f64)).log10()
}

/// Which tiles of a 64-pixel grid over `width` by `height` a set of rectangles covers, exactly once each.
fn covered(rects: &[Rect], width: u32, height: u32) -> Vec<(u32, u32)> {
    let mut seen = vec![0_u8; (width.div_ceil(64) * height.div_ceil(64)) as usize];
    for rect in rects {
        assert!(rect.fits(width, height), "{rect:?}");
        assert_eq!(rect.x % 64, 0);
        assert_eq!(rect.y % 64, 0);
        for ty in rect.y / 64..(rect.y + rect.height).div_ceil(64) {
            for tx in rect.x / 64..(rect.x + rect.width).div_ceil(64) {
                seen[(ty * width.div_ceil(64) + tx) as usize] += 1;
            }
        }
    }
    let across = width.div_ceil(64);
    let mut tiles = Vec::new();
    for (index, count) in seen.iter().enumerate() {
        assert!(*count <= 1, "a tile is covered twice");
        if *count == 1 {
            tiles.push((index as u32 % across, index as u32 / across));
        }
    }
    tiles
}

#[test]
fn diff_reports_exactly_the_changed_tiles() {
    let (width, height) = (200, 150);
    let before = picture(width, height, 1);
    let mut grid = TileGrid::new(width, height, 64);
    let first = grid.diff(&Pixels::new(width, height, &before).unwrap());
    assert_eq!(first, vec![Rect::new(0, 0, width, height)]);
    assert!(
        grid.diff(&Pixels::new(width, height, &before).unwrap())
            .is_empty()
    );

    let mut after = before.clone();
    // One pixel in tile (1, 0), one in the narrow last column (3, 2), and the same pixel again in tile (1, 0).
    set(&mut after, width, 70, 3, [1, 2, 3]);
    set(&mut after, width, 199, 149, [9, 9, 9]);
    set(&mut after, width, 71, 4, [1, 2, 3]);
    let rects = grid.diff(&Pixels::new(width, height, &after).unwrap());
    assert_eq!(covered(&rects, width, height), vec![(1, 0), (3, 2)]);
    // The last tile is cut by the frame's edge.
    assert!(rects.contains(&Rect::new(192, 128, 8, 22)));
    // Nothing changed again.
    assert!(
        grid.diff(&Pixels::new(width, height, &after).unwrap())
            .is_empty()
    );
}

#[test]
fn touching_tiles_merge_into_runs_and_blocks() {
    let (width, height) = (256, 256);
    let mut grid = TileGrid::new(width, height, 64);
    let base = vec![0_u8; width as usize * height as usize * 3];
    grid.diff(&Pixels::new(width, height, &base).unwrap());
    let mut changed = base.clone();
    // A 2 by 2 block of tiles at (1, 1), one tile at (0, 3), one at (3, 3), and one at (3, 0).
    for (tx, ty) in [(1, 1), (2, 1), (1, 2), (2, 2), (0, 3), (3, 3), (3, 0)] {
        set(&mut changed, width, tx * 64 + 5, ty * 64 + 5, [200, 0, 0]);
    }
    let rects = grid.diff(&Pixels::new(width, height, &changed).unwrap());
    assert!(rects.contains(&Rect::new(64, 64, 128, 128)), "{rects:?}");
    assert_eq!(rects.len(), 4, "{rects:?}");
    assert_eq!(
        covered(&rects, width, height),
        vec![(3, 0), (1, 1), (2, 1), (1, 2), (2, 2), (0, 3), (3, 3)]
    );
}

#[test]
fn a_size_change_reports_the_whole_frame() {
    let mut grid = TileGrid::new(128, 128, 64);
    let small = picture(128, 128, 0);
    grid.diff(&Pixels::new(128, 128, &small).unwrap());
    let big = picture(130, 66, 0);
    let rects = grid.diff(&Pixels::new(130, 66, &big).unwrap());
    assert_eq!(rects, vec![Rect::new(0, 0, 130, 66)]);
}

#[test]
fn each_encoding_round_trips() {
    let (width, height) = (130, 70);
    let rgb = picture(width, height, 3);
    let frame = Pixels::new(width, height, &rgb).unwrap();
    let rect = Rect::new(10, 6, 100, 60);
    let want: Vec<u8> = (6..66)
        .flat_map(|y| {
            let from = ((y * width + 10) * 3) as usize;
            rgb[from..from + 300].to_vec()
        })
        .collect();
    let params = EncodeParams::default();
    for encoding in [
        RectEncoding::RawRgb,
        RectEncoding::ZstdRgb,
        RectEncoding::Png,
    ] {
        let bytes = encode_rect(&frame, rect, encoding, &params).unwrap();
        assert_eq!(
            decode_rect(&bytes, rect, encoding).unwrap(),
            want,
            "{encoding}"
        );
    }
    let zstd = encode_rect(&frame, rect, RectEncoding::ZstdRgb, &params).unwrap();
    assert!(zstd.len() < want.len(), "zstd compresses a picture");
    let jpeg = encode_rect(
        &frame,
        rect,
        RectEncoding::Jpeg,
        &EncodeParams {
            quality: 85,
            ..params
        },
    )
    .unwrap();
    let back = decode_rect(&jpeg, rect, RectEncoding::Jpeg).unwrap();
    let quality = psnr(&want, &back);
    assert!(quality >= 30.0, "JPEG PSNR {quality:.1} dB");
    assert!(jpeg.len() < want.len());
    // A lower quality is smaller.
    let rough = encode_rect(
        &frame,
        rect,
        RectEncoding::Jpeg,
        &EncodeParams {
            quality: 20,
            ..params
        },
    )
    .unwrap();
    assert!(rough.len() < jpeg.len());
}

#[test]
fn a_rectangle_that_is_not_what_its_encoding_says_is_refused() {
    let rect = Rect::new(0, 0, 8, 8);
    let rgb = picture(8, 8, 0);
    let frame = Pixels::new(8, 8, &rgb).unwrap();
    let params = EncodeParams::default();
    for encoding in [
        RectEncoding::RawRgb,
        RectEncoding::ZstdRgb,
        RectEncoding::Jpeg,
        RectEncoding::Png,
    ] {
        let good = encode_rect(&frame, rect, encoding, &params).unwrap();
        // Another size than the rectangle's.
        assert!(
            decode_rect(&good, Rect::new(0, 0, 9, 8), encoding).is_err(),
            "{encoding} read as another size"
        );
        assert!(
            decode_rect(&good[..good.len() / 2], rect, encoding).is_err(),
            "{encoding} cut"
        );
        assert!(
            decode_rect(&[], rect, encoding).is_err(),
            "{encoding} empty"
        );
    }
    // A rectangle outside the frame cannot be encoded.
    assert_eq!(
        encode_rect(&frame, Rect::new(4, 4, 8, 8), RectEncoding::RawRgb, &params),
        Err(ScreenError::OutsideFrame)
    );
}

/// What a host sends for `rects` of `frame`.
fn update(
    frame: &Pixels<'_>,
    rects: &[Rect],
    encoding: RectEncoding,
    key: bool,
    number: u64,
) -> (UpdateHeader, Vec<u8>) {
    let params = EncodeParams::default();
    let mut bytes = Vec::new();
    let mut parts = Vec::new();
    for rect in rects {
        let encoded = encode_rect(frame, *rect, encoding, &params).unwrap();
        parts.push(UpdateRect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            encoding,
            length: encoded.len() as u64,
        });
        bytes.extend_from_slice(&encoded);
    }
    let header = UpdateHeader {
        frame: number,
        display: DisplayId::from("0"),
        width: frame.width(),
        height: frame.height(),
        scale: 1.0,
        key,
        rects: parts,
        cursor: None,
        level: 0,
        ack_ms: None,
    };
    (header, bytes)
}

#[test]
fn a_compositor_fed_a_recorded_sequence_equals_the_source() {
    let (width, height) = (200, 150);
    let mut source = picture(width, height, 2);
    let mut grid = TileGrid::new(width, height, 64);
    let mut compositor = Compositor::new(Limits::default());
    assert!(compositor.frame().is_none());
    let mut number = 0;
    for step in 0..6_u32 {
        if step > 0 {
            // A moving square and a line of typing.
            for y in 0..30 {
                for x in 0..30 {
                    set(
                        &mut source,
                        width,
                        10 + step * 20 + x,
                        40 + y,
                        [255, step as u8 * 40, 0],
                    );
                }
            }
            for x in 0..40 {
                set(&mut source, width, 120 + x, 130, [step as u8; 3]);
            }
        }
        let frame = Pixels::new(width, height, &source).unwrap();
        let rects = grid.diff(&frame);
        assert!(!rects.is_empty());
        number += 1;
        let (header, bytes) = update(&frame, &rects, RectEncoding::ZstdRgb, step == 0, number);
        compositor.apply(&header, &bytes).unwrap();
        assert_eq!(
            compositor.frame().unwrap().rgb(),
            source.as_slice(),
            "step {step}"
        );
    }
}

#[test]
fn a_hostile_update_is_refused_and_does_not_grow_the_client() {
    let (width, height) = (64, 64);
    let rgb = picture(width, height, 0);
    let frame = Pixels::new(width, height, &rgb).unwrap();
    let whole = [Rect::new(0, 0, width, height)];
    let limits = Limits {
        max_rect_bytes: 1000,
        max_frame_pixels: 64 * 64,
    };

    // A rectangle longer than the bound, whatever the bytes are.
    let (mut header, bytes) = update(&frame, &whole, RectEncoding::RawRgb, true, 1);
    assert_eq!(
        Compositor::new(limits).apply(&header, &bytes),
        Err(ScreenError::TooLarge)
    );
    // Lengths that do not add up to the bytes.
    let mut roomy = Compositor::new(Limits::default());
    header.rects[0].length -= 1;
    assert!(matches!(
        roomy.apply(&header, &bytes),
        Err(ScreenError::Inconsistent(_))
    ));
    // A rectangle outside the frame.
    let (mut header, bytes) = update(&frame, &whole, RectEncoding::RawRgb, true, 1);
    header.rects[0].x = 1;
    assert_eq!(roomy.apply(&header, &bytes), Err(ScreenError::OutsideFrame));
    // A frame of more pixels than the client holds.
    let (mut header, bytes) = update(&frame, &whole, RectEncoding::RawRgb, true, 1);
    header.width = 100_000;
    header.height = 100_000;
    assert_eq!(
        Compositor::new(Limits::default()).apply(&header, &bytes),
        Err(ScreenError::TooLarge)
    );
    // A change of size that is not a key frame, and a key frame that leaves a part out.
    let (header, bytes) = update(&frame, &whole, RectEncoding::RawRgb, false, 1);
    assert!(matches!(
        Compositor::new(Limits::default()).apply(&header, &bytes),
        Err(ScreenError::Inconsistent(_))
    ));
    let part = [Rect::new(0, 0, 32, 32)];
    let (header, bytes) = update(&frame, &part, RectEncoding::RawRgb, true, 1);
    assert!(matches!(
        Compositor::new(Limits::default()).apply(&header, &bytes),
        Err(ScreenError::Inconsistent(_))
    ));
    // A rectangle whose bytes claim a larger picture than its place.
    let big = picture(128, 128, 0);
    let big_frame = Pixels::new(128, 128, &big).unwrap();
    let png = encode_rect(
        &big_frame,
        Rect::new(0, 0, 128, 128),
        RectEncoding::Png,
        &EncodeParams::default(),
    )
    .unwrap();
    let header = UpdateHeader {
        frame: 1,
        display: DisplayId::from("0"),
        width: 64,
        height: 64,
        scale: 1.0,
        key: true,
        rects: vec![UpdateRect {
            x: 0,
            y: 0,
            width: 64,
            height: 64,
            encoding: RectEncoding::Png,
            length: png.len() as u64,
        }],
        cursor: None,
        level: 0,
        ack_ms: None,
    };
    assert!(matches!(
        Compositor::new(Limits::default()).apply(&header, &png),
        Err(ScreenError::Corrupt(_))
    ));
    // A zstd rectangle that expands past its place (a decompression bomb) stops at the size.
    let zeros = vec![0_u8; 256 * 256 * 3];
    let bomb = encode_rect(
        &Pixels::new(256, 256, &zeros).unwrap(),
        Rect::new(0, 0, 256, 256),
        RectEncoding::ZstdRgb,
        &EncodeParams::default(),
    )
    .unwrap();
    assert!(bomb.len() < 1000);
    assert!(decode_rect(&bomb, Rect::new(0, 0, 64, 64), RectEncoding::ZstdRgb).is_err());
}
