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
        eprintln!("bohio: {text}");
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
        eprintln!("bohio: {message}");
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
