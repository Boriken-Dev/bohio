//! The pattern `gui_probe_x11` paints and `tests/gui_x11.rs` expects: eight-pixel blocks of six colours, so that a capture
//! shifted by one pixel anywhere differs.

pub const WIDTH: u16 = 128;
pub const HEIGHT: u16 = 96;
pub const BLOCK: u16 = 8;
pub const PALETTE: [(u8, u8, u8); 6] = [
    (255, 0, 0),
    (0, 255, 0),
    (0, 0, 255),
    (255, 255, 0),
    (0, 255, 255),
    (255, 0, 255),
];

/// The colour of the block that holds the pixel `x`, `y`.
pub fn colour(x: u16, y: u16) -> (u8, u8, u8) {
    let (bx, by) = (usize::from(x / BLOCK), usize::from(y / BLOCK));
    PALETTE[(bx * 7 + by * 3 + bx * by) % PALETTE.len()]
}

/// The RGB bytes of the whole window.
#[allow(
    dead_code,
    reason = "the test builds the expected picture; the probe paints blocks"
)]
pub fn rgb() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(usize::from(WIDTH) * usize::from(HEIGHT) * 3);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let (r, g, b) = colour(x, y);
            bytes.extend_from_slice(&[r, g, b]);
        }
    }
    bytes
}
