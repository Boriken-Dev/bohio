//! The deterministic corpus the scenarios read: incompressible, text and many small files.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

/// The bytes of a size written as `1KiB`, `16MiB`, `1GiB` or a bare number.
pub fn parse_size(text: &str) -> Result<u64, String> {
    let (digits, factor) = if let Some(rest) = text.strip_suffix("GiB") {
        (rest, 1_u64 << 30)
    } else if let Some(rest) = text.strip_suffix("MiB") {
        (rest, 1 << 20)
    } else if let Some(rest) = text.strip_suffix("KiB") {
        (rest, 1 << 10)
    } else {
        (text, 1)
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|count| count.checked_mul(factor))
        .ok_or_else(|| format!("`{text}` is not a size (1KiB, 16MiB, 1GiB or bytes)"))
}

/// A xorshift64 generator: the same sequence everywhere.
pub struct Xorshift(u64);

impl Xorshift {
    /// The generator every corpus file starts from.
    pub const fn fixed() -> Self {
        Self(0x9E37_79B9_7F4A_7C15)
    }

    /// A generator from `seed` (not zero).
    pub const fn new(seed: u64) -> Self {
        Self(if seed == 0 { 1 } else { seed })
    }

    /// The next 64 bits.
    pub const fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Fills `buffer` with the next bytes.
    pub fn fill(&mut self, buffer: &mut [u8]) {
        for chunk in buffer.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}

/// `size` bytes of the fixed sequence to `file`.
fn random_file(path: &Path, size: u64) -> std::io::Result<()> {
    let mut file = BufWriter::new(File::create(path)?);
    let mut generator = Xorshift::fixed();
    let mut block = vec![0_u8; 1 << 20];
    let mut left = size;
    while left > 0 {
        let take = usize::try_from(left.min(block.len() as u64)).unwrap_or(block.len());
        generator.fill(&mut block[..take]);
        file.write_all(&block[..take])?;
        left -= take as u64;
    }
    file.flush()
}

/// `size` bytes of words and numbers from a generated list: text that compresses.
fn text_file(path: &Path, size: u64) -> std::io::Result<()> {
    let mut generator = Xorshift::new(0xC0FF_EE00_D15E_A5E5);
    let words: Vec<String> = (0..400)
        .map(|_| {
            let length = 2 + (generator.next_u64() % 9) as usize;
            (0..length)
                .map(|_| char::from(b'a' + (generator.next_u64() % 26) as u8))
                .collect()
        })
        .collect();
    let mut file = BufWriter::new(File::create(path)?);
    let mut written = 0_u64;
    let mut line = String::new();
    while written < size {
        line.clear();
        for _ in 0..12 {
            let pick = generator.next_u64();
            line.push_str(&words[(pick % words.len() as u64) as usize]);
            line.push(' ');
            if pick % 5 < 1 {
                line.push_str(&format!("{} ", (pick >> 20) % 100_000));
            }
        }
        line.push('\n');
        let take = usize::try_from(size - written).map_or(line.len(), |left| left.min(line.len()));
        file.write_all(&line.as_bytes()[..take])?;
        written += take as u64;
    }
    file.flush()
}

/// Makes the corpus in `dir`: `random-<size>` and `text-<size>` for each size, and `small/<n>` of
/// 1 to 8 KiB each. A file that already has its size is kept.
///
/// # Errors
///
/// The error of creating a directory or a file, or a size that does not parse.
pub fn make(dir: &Path, sizes: &[String], small: usize) -> Result<(), String> {
    std::fs::create_dir_all(dir.join("small")).map_err(|error| error.to_string())?;
    for text in sizes {
        let size = parse_size(text)?;
        for (kind, make) in [
            (
                "random",
                random_file as fn(&Path, u64) -> std::io::Result<()>,
            ),
            ("text", text_file),
        ] {
            let path = dir.join(format!("{kind}-{text}"));
            if std::fs::metadata(&path).is_ok_and(|found| found.len() == size) {
                continue;
            }
            make(&path, size).map_err(|error| format!("{}: {error}", path.display()))?;
        }
    }
    let mut generator = Xorshift::new(0x5151_5151_A5A5_A5A5);
    for index in 0..small {
        let size = 1024 + generator.next_u64() % (7 * 1024 + 1);
        let path = dir.join("small").join(format!("{index:05}.bin"));
        if std::fs::metadata(&path).is_ok_and(|found| found.len() == size) {
            continue;
        }
        let mut bytes = vec![0_u8; usize::try_from(size).unwrap_or(1024)];
        generator.fill(&mut bytes);
        std::fs::write(&path, bytes).map_err(|error| format!("{}: {error}", path.display()))?;
    }
    Ok(())
}
