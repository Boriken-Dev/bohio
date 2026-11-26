//! How a command writes: text for a person, or with `--json` objects a program reads.
//!
//! One process runs one command, so the mode is a process-wide flag set once, after the arguments are read. In JSON mode a
//! command prints one JSON object on standard output, or for a streaming command one object per line (each with an
//! `event` key); a command whose output is the payload (a file, a connection, a terminal) writes it unchanged and puts
//! its final status on standard error as one object; every line standard error then holds is one JSON object, and an
//! error is `{"error": {"code": ..., "message": ...}}`. `docs/cli-json.md` gives every shape.

#![forbid(unsafe_code)]

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};

use serde_json::{Value, json};

static JSON: AtomicBool = AtomicBool::new(false);
static CODE: Mutex<Option<String>> = Mutex::new(None);

/// What a person's terminal may be shown of text a host sent: control characters (C0, DEL and C1, so an escape sequence, a
/// carriage return or a bell) are written as `\xNN` or `\u{..}`, and a tab and, with `lines`, a newline are kept. JSON
/// mode needs none of this: it escapes them itself.
fn escaped(text: &str, lines: bool) -> std::borrow::Cow<'_, str> {
    let bad = |c: char| match c {
        '\t' => false,
        '\n' => !lines,
        c => c.is_control(),
    };
    if !text.chars().any(bad) {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for c in text.chars() {
        if !bad(c) {
            out.push(c);
        } else if (c as u32) < 0x100 && !(0x80..0xa0).contains(&(c as u32)) {
            out.push_str(&format!("\\x{:02x}", c as u32));
        } else {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        }
    }
    std::borrow::Cow::Owned(out)
}

/// One item of a host's text (a name, a path, a field) for a person's terminal: control characters, a newline too, are
/// escaped, so it cannot move the cursor, change the title, fake another line or set the clipboard.
pub(super) fn shown(text: &str) -> std::borrow::Cow<'_, str> {
    escaped(text, false)
}

/// As [`shown`] for a message that may run over several lines: newlines stay.
pub(super) fn shown_lines(text: &str) -> std::borrow::Cow<'_, str> {
    escaped(text, true)
}

/// Selects JSON output for the rest of the process.
pub(super) fn set_json(on: bool) {
    JSON.store(on, Ordering::SeqCst);
}

/// Whether the command was given `--json`.
pub(super) fn json() -> bool {
    JSON.load(Ordering::SeqCst)
}

/// Prints `value` as one line on standard output, flushed: a program reads these lines as they happen.
pub(super) fn object(value: &Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

/// Prints one event of a streaming command: `fields` (an object) with `event` first.
pub(super) fn event(name: &str, fields: Value) {
    let mut line = serde_json::Map::new();
    line.insert("event".to_owned(), json!(name));
    if let Value::Object(fields) = fields {
        line.extend(fields);
    }
    object(&Value::Object(line));
}

/// Prints `value` as one line on standard error, flushed.
pub(super) fn status(value: &Value) {
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{value}");
    let _ = err.flush();
}

/// A line on standard error for a person: `bohio: TEXT`, or in JSON mode `{"note": TEXT}`.
pub(super) fn note(text: &str) {
    if json() {
        status(&json!({ "note": text }));
    } else {
        eprintln!("bohio: {}", shown_lines(text));
    }
}

/// Records the code of the error the command is about to fail with (`remote` errors name the host's, others a kind).
pub(super) fn code(code: &str) {
    *CODE.lock().unwrap_or_else(PoisonError::into_inner) = Some(code.to_owned());
}

/// The error that ends a command, as standard error shows it: `bohio: MESSAGE`, or in JSON mode `{"error": {"code",
/// "message"}}` with the code the failing call recorded (`failed` when none did).
pub(super) fn fail(message: &str) {
    if json() {
        let code = CODE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_else(|| "failed".to_owned());
        status(&json!({ "error": { "code": code, "message": message } }));
    } else {
        eprintln!("bohio: {}", shown_lines(message));
    }
}

/// Bytes as the standard base64 alphabet writes them, for the `b64` of a payload inside an event.
pub(super) fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut text = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        text.push(char::from(TABLE[(n >> 18) as usize & 63]));
        text.push(char::from(TABLE[(n >> 12) as usize & 63]));
        text.push(if chunk.len() > 1 {
            char::from(TABLE[(n >> 6) as usize & 63])
        } else {
            '='
        });
        text.push(if chunk.len() > 2 {
            char::from(TABLE[n as usize & 63])
        } else {
            '='
        });
    }
    text
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_is_the_standard_alphabet_with_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        assert_eq!(base64(&[0xfb, 0xff]), "+/8=");
    }
}

#[cfg(test)]
mod escape_tests {
    use super::*;

    #[test]
    fn control_characters_are_written_out_and_plain_text_is_not_touched() {
        assert_eq!(shown("plain, tab\there"), "plain, tab\there");
        assert_eq!(shown("a\x1b]52;c;x\x07b"), r"a\x1b]52;c;x\x07b");
        assert_eq!(shown("a\r\nb\x7f"), r"a\x0d\x0ab\x7f");
        assert_eq!(shown("c1\u{85}\u{9b}"), r"c1\u{85}\u{9b}");
        assert_eq!(shown("héllo ✓"), "héllo ✓");
        // A message may run over lines; its newlines stay and its escapes do not.
        assert_eq!(shown_lines("one\ntwo\x1b[2J"), "one\ntwo\\x1b[2J");
    }
}
