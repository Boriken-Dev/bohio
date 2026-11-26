//! The byte layouts `winfs` hands to the system or reads back: a `FILE_RENAME_INFORMATION`, a reparse
//! point's data and the entries of a directory listing. Plain buffer work with no operating-system call, so
//! it builds and is tested on every platform and holds no `unsafe`; `winfs` passes the buffers to the calls.
//!
//! The system reads and writes these buffers as structures that hold a pointer or a 64-bit number, so each
//! lives in eight-byte units: [`aligned`] copies bytes into them and [`bytes_of`] copies a filled buffer back
//! out. Integers are little-endian, as on every Windows target.

#![forbid(unsafe_code)]

use std::io;

/// A reparse point of a mount point (a junction).
pub(super) const IO_REPARSE_TAG_MOUNT_POINT: u32 = 0xA000_0003;
/// A reparse point of a symbolic link.
pub(super) const IO_REPARSE_TAG_SYMLINK: u32 = 0xA000_000C;
/// The symbolic link's target is relative to the link's directory.
pub(super) const SYMLINK_FLAG_RELATIVE: u32 = 1;
/// The most reparse data a link is made with or read through.
pub(super) const MAX_REPARSE_DATA: usize = 16 * 1024;

/// The offset of `RootDirectory` in a `FILE_RENAME_INFORMATION`: after the four-byte flags, at pointer alignment.
pub(super) const RENAME_ROOT: usize = size_of::<usize>();
/// The offset of `FileNameLength` in a `FILE_RENAME_INFORMATION`.
pub(super) const RENAME_LENGTH: usize = RENAME_ROOT + size_of::<usize>();
/// The offset of `FileName` in a `FILE_RENAME_INFORMATION`.
pub(super) const RENAME_NAME: usize = RENAME_LENGTH + 4;

/// The offset of `FileAttributes` in a `FILE_FULL_DIR_INFO`.
pub(super) const ENTRY_ATTRIBUTES: usize = 56;
/// The offset of `FileNameLength` in a `FILE_FULL_DIR_INFO`.
pub(super) const ENTRY_NAME_LENGTH: usize = 60;
/// The offset of `FileName` in a `FILE_FULL_DIR_INFO`.
pub(super) const ENTRY_NAME: usize = 68;

/// The error of a name, or a link target, that cannot be used.
pub(super) fn bad_name() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "a name must be one component without a separator or a stream",
    )
}

/// The error of data the system returned that does not parse.
pub(super) fn malformed() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "the reparse data is malformed")
}

/// A buffer in eight-byte units, and how many of its bytes are the data.
#[derive(Debug)]
pub(super) struct Packed {
    pub(super) words: Vec<u64>,
    pub(super) len: usize,
}

/// `bytes` in eight-byte units, zero-padded to a whole unit, so the buffer is aligned for a `HANDLE`.
pub(super) fn aligned(bytes: &[u8]) -> Vec<u64> {
    bytes
        .chunks(8)
        .map(|chunk| {
            let mut word = [0_u8; 8];
            word[..chunk.len()].copy_from_slice(chunk);
            u64::from_ne_bytes(word)
        })
        .collect()
}

/// The bytes of eight-byte units, in memory order.
pub(super) fn bytes_of(words: &[u64]) -> Vec<u8> {
    words.iter().flat_map(|word| word.to_ne_bytes()).collect()
}

fn put(buffer: &mut [u8], at: usize, value: &[u8]) {
    buffer[at..at + value.len()].copy_from_slice(value);
}

/// A `FILE_RENAME_INFORMATION` whose first field is `first` (the flags of the extended form, `ReplaceIfExists`
/// of the classic one), moving to `name` inside the directory `root`.
pub(super) fn rename_information(first: u32, root: usize, name: &[u16]) -> io::Result<Packed> {
    let name_bytes = name.len() * 2;
    let len = RENAME_NAME + name_bytes + 2;
    let mut buffer = vec![0_u8; len];
    put(&mut buffer, 0, &first.to_le_bytes());
    put(&mut buffer, RENAME_ROOT, &root.to_le_bytes());
    let length = u32::try_from(name_bytes).map_err(|_| bad_name())?;
    put(&mut buffer, RENAME_LENGTH, &length.to_le_bytes());
    for (index, unit) in name.iter().enumerate() {
        put(&mut buffer, RENAME_NAME + index * 2, &unit.to_le_bytes());
    }
    Ok(Packed {
        words: aligned(&buffer),
        len,
    })
}

/// The data of a symbolic link to `printed`: an absolute target is stored with the `\??\` prefix in the
/// substitute name and without it in the printed one.
pub(super) fn symlink_data(printed: &[u16], absolute: bool) -> io::Result<Packed> {
    let substitute: Vec<u16> = if absolute {
        r"\??\"
            .encode_utf16()
            .chain(printed.iter().copied())
            .collect()
    } else {
        printed.to_vec()
    };
    let (sub_bytes, print_bytes) = (substitute.len() * 2, printed.len() * 2);
    let data_length = 12 + sub_bytes + print_bytes;
    if data_length > MAX_REPARSE_DATA {
        return Err(bad_name());
    }
    let field = |value: usize| u16::try_from(value).map_err(|_| bad_name());
    let len = 8 + data_length;
    let mut buffer = vec![0_u8; len];
    put(&mut buffer, 0, &IO_REPARSE_TAG_SYMLINK.to_le_bytes());
    put(&mut buffer, 4, &field(data_length)?.to_le_bytes());
    put(&mut buffer, 8, &0_u16.to_le_bytes());
    put(&mut buffer, 10, &field(sub_bytes)?.to_le_bytes());
    put(&mut buffer, 12, &field(sub_bytes)?.to_le_bytes());
    put(&mut buffer, 14, &field(print_bytes)?.to_le_bytes());
    let flags = if absolute { 0 } else { SYMLINK_FLAG_RELATIVE };
    put(&mut buffer, 16, &flags.to_le_bytes());
    for (index, unit) in substitute.iter().chain(printed.iter()).enumerate() {
        put(&mut buffer, 20 + index * 2, &unit.to_le_bytes());
    }
    Ok(Packed {
        words: aligned(&buffer),
        len,
    })
}

/// The data of a mount point (a junction) to the substitute name `substitute`, as a test makes one.
#[cfg(test)]
pub(super) fn mount_point_data(substitute: &[u16]) -> io::Result<Packed> {
    let sub_bytes = substitute.len() * 2;
    let data_length = 8 + sub_bytes + 2 + 2;
    let len = 8 + data_length;
    let field = |value: usize| u16::try_from(value).map_err(|_| bad_name());
    let mut buffer = vec![0_u8; len];
    put(&mut buffer, 0, &IO_REPARSE_TAG_MOUNT_POINT.to_le_bytes());
    put(&mut buffer, 4, &field(data_length)?.to_le_bytes());
    put(&mut buffer, 8, &0_u16.to_le_bytes());
    put(&mut buffer, 10, &field(sub_bytes)?.to_le_bytes());
    put(&mut buffer, 12, &field(sub_bytes + 2)?.to_le_bytes());
    put(&mut buffer, 14, &0_u16.to_le_bytes());
    for (index, unit) in substitute.iter().enumerate() {
        put(&mut buffer, 16 + index * 2, &unit.to_le_bytes());
    }
    Ok(Packed {
        words: aligned(&buffer),
        len,
    })
}

fn read_u16(buffer: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(buffer.get(at..at + 2)?.try_into().ok()?))
}

fn read_u32(buffer: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(buffer.get(at..at + 4)?.try_into().ok()?))
}

fn units(text: &[u8]) -> Vec<u16> {
    text.as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect()
}

/// The text of the link in the first `returned` bytes of `storage`, which the system filled: a relative
/// target as it was written, an absolute one without the `\??\` prefix.
pub(super) fn link_text(storage: &[u64], returned: u32) -> io::Result<Vec<u16>> {
    let all = bytes_of(storage);
    let length = usize::try_from(returned)
        .unwrap_or(0)
        .min(MAX_REPARSE_DATA)
        .min(all.len());
    let buffer = &all[..length];
    let tag = read_u32(buffer, 0).ok_or_else(malformed)?;
    let (names_at, relative) = match tag {
        IO_REPARSE_TAG_SYMLINK => (
            20,
            read_u32(buffer, 16).ok_or_else(malformed)? & SYMLINK_FLAG_RELATIVE != 0,
        ),
        IO_REPARSE_TAG_MOUNT_POINT => (16, false),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "an unsupported kind of reparse point",
            ));
        }
    };
    let offset = usize::from(read_u16(buffer, 8).ok_or_else(malformed)?);
    let length = usize::from(read_u16(buffer, 10).ok_or_else(malformed)?);
    let text = buffer
        .get(names_at + offset..names_at + offset + length)
        .ok_or_else(malformed)?;
    let mut wide = units(text);
    let prefix: Vec<u16> = r"\??\".encode_utf16().collect();
    if !relative && wide.starts_with(&prefix) {
        wide.drain(..prefix.len());
    }
    Ok(wide)
}

/// One entry of a directory listing.
pub(super) struct Entry {
    /// The distance to the next entry; zero for the last of a buffer.
    pub(super) next: usize,
    pub(super) attributes: u32,
    pub(super) name: Vec<u16>,
}

/// The entry of `buffer` that starts at `at`, or `None` when it does not lie inside the buffer.
pub(super) fn dir_entry(buffer: &[u8], at: usize) -> Option<Entry> {
    let entry = buffer.get(at..)?;
    let next = usize::try_from(read_u32(entry, 0)?).ok()?;
    let attributes = read_u32(entry, ENTRY_ATTRIBUTES)?;
    let length = usize::try_from(read_u32(entry, ENTRY_NAME_LENGTH)?).ok()?;
    let text = entry.get(ENTRY_NAME..ENTRY_NAME.checked_add(length)?)?;
    Some(Entry {
        next,
        attributes,
        name: units(text),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    fn le32(buffer: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(buffer[at..at + 4].try_into().unwrap())
    }

    #[test]
    fn units_round_trip_through_bytes_and_pad_to_a_whole_unit() {
        let bytes: Vec<u8> = (1..=13).collect();
        let words = aligned(&bytes);
        assert_eq!(words.len(), 2);
        let back = bytes_of(&words);
        assert_eq!(&back[..13], &bytes[..]);
        assert!(back[13..].iter().all(|byte| *byte == 0));
        assert!(aligned(&[]).is_empty());
    }

    #[test]
    fn the_rename_buffer_holds_what_the_kernel_reads() {
        let name = wide("moved.txt");
        let root = 0x1234_usize;
        let packed = rename_information(0x3, root, &name).unwrap();
        assert_eq!(packed.len, RENAME_NAME + name.len() * 2 + 2);
        assert!(packed.words.len() * 8 >= packed.len);
        let bytes = bytes_of(&packed.words);
        assert_eq!(le32(&bytes, 0), 0x3);
        assert_eq!(
            usize::from_le_bytes(
                bytes[RENAME_ROOT..RENAME_ROOT + size_of::<usize>()]
                    .try_into()
                    .unwrap()
            ),
            root
        );
        let length = usize::try_from(le32(&bytes, RENAME_LENGTH)).unwrap();
        assert_eq!(length, name.len() * 2);
        assert_eq!(units(&bytes[RENAME_NAME..RENAME_NAME + length]), name);
        assert_eq!(&bytes[RENAME_NAME + length..packed.len], &[0, 0]);
    }

    #[test]
    fn a_link_round_trips_through_its_data() {
        for (target, absolute) in [("sub\\file.txt", false), ("C:\\data\\file.txt", true)] {
            let printed = wide(target);
            let packed = symlink_data(&printed, absolute).unwrap();
            let read = link_text(&packed.words, u32::try_from(packed.len).unwrap()).unwrap();
            assert_eq!(read, printed, "{target}");
        }
    }

    #[test]
    fn a_mount_point_round_trips_and_loses_its_prefix() {
        let substitute = wide(r"\??\C:\some\dir");
        let packed = mount_point_data(&substitute).unwrap();
        let read = link_text(&packed.words, u32::try_from(packed.len).unwrap()).unwrap();
        assert_eq!(read, wide(r"C:\some\dir"));
    }

    #[test]
    fn a_target_past_the_most_reparse_data_is_refused() {
        let long = vec![u16::from(b'a'); MAX_REPARSE_DATA];
        let error = symlink_data(&long, false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn link_data_with_lengths_past_the_buffer_is_malformed() {
        let packed = symlink_data(&wide("x"), false).unwrap();
        let valid = u32::try_from(packed.len).unwrap();
        // An offset or a length of 0xFFFF reaches past any buffer.
        for at in [8, 10] {
            let mut bytes = bytes_of(&packed.words);
            bytes[at..at + 2].copy_from_slice(&0xFFFF_u16.to_le_bytes());
            let error = link_text(&aligned(&bytes), valid).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "field at {at}");
        }
        // Fewer bytes returned than the data says are there.
        let error = link_text(&packed.words, 6).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        let error = link_text(&packed.words, 0).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn a_kind_of_reparse_point_that_is_neither_is_unsupported() {
        let mut bytes = bytes_of(&symlink_data(&wide("x"), false).unwrap().words);
        bytes[..4].copy_from_slice(&0x8000_0017_u32.to_le_bytes());
        let error = link_text(&aligned(&bytes), 64).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    /// A chain of entries as the system writes it: each starts on an eight-byte boundary and the last says zero.
    fn written(names: &[&str], words: usize) -> Vec<u8> {
        let mut out = vec![0_u8; words * 8];
        let mut at = 0;
        for (index, name) in names.iter().enumerate() {
            let text = wide(name);
            let size = (ENTRY_NAME + text.len() * 2).next_multiple_of(8);
            let next = if index + 1 == names.len() { 0 } else { size };
            put(&mut out, at, &u32::try_from(next).unwrap().to_le_bytes());
            put(&mut out, at + ENTRY_ATTRIBUTES, &0x10_u32.to_le_bytes());
            let length = u32::try_from(text.len() * 2).unwrap();
            put(&mut out, at + ENTRY_NAME_LENGTH, &length.to_le_bytes());
            for (unit_at, unit) in text.iter().enumerate() {
                put(&mut out, at + ENTRY_NAME + unit_at * 2, &unit.to_le_bytes());
            }
            at += size;
        }
        out
    }

    /// The names of a buffer walked as `Listing` walks it.
    fn walk(buffer: &[u8]) -> Option<Vec<String>> {
        let mut at = 0;
        let mut names = Vec::new();
        while at < buffer.len() {
            let entry = dir_entry(buffer, at)?;
            at = if entry.next == 0 {
                buffer.len()
            } else {
                at + entry.next
            };
            names.push(String::from_utf16_lossy(&entry.name));
        }
        Some(names)
    }

    #[test]
    fn a_written_chain_is_walked_to_its_last_entry() {
        let buffer = written(&[".", "..", "alpha", "beta.txt", "g"], 64);
        assert_eq!(
            walk(&buffer).unwrap(),
            [".", "..", "alpha", "beta.txt", "g"]
        );
        let entry = dir_entry(&buffer, 0).unwrap();
        assert_eq!(entry.attributes, 0x10);
    }

    #[test]
    fn an_entry_whose_name_reaches_past_the_buffer_is_refused() {
        let mut buffer = written(&["alpha"], 16);
        let past = u32::try_from(buffer.len()).unwrap();
        put(&mut buffer, ENTRY_NAME_LENGTH, &past.to_le_bytes());
        assert!(dir_entry(&buffer, 0).is_none());
        put(&mut buffer, ENTRY_NAME_LENGTH, &u32::MAX.to_le_bytes());
        assert!(dir_entry(&buffer, 0).is_none());
        // A header that does not fit, and a start past the end.
        assert!(dir_entry(&buffer[..ENTRY_NAME - 1], 0).is_none());
        assert!(dir_entry(&buffer, buffer.len() + 1).is_none());
    }

    #[test]
    fn a_next_offset_inside_an_entry_stays_inside_the_buffer() {
        let mut buffer = written(&["alpha", "beta"], 32);
        put(&mut buffer, 0, &4_u32.to_le_bytes());
        let _ = walk(&buffer);
        put(&mut buffer, 0, &u32::MAX.to_le_bytes());
        let _ = walk(&buffer);
    }
}
