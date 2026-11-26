//! What a host says reaches the person's terminal without control characters: a name or a message cannot set the title,
//! move the cursor, fake another line or ask the terminal for its clipboard.

#![forbid(unsafe_code)]
#![cfg(unix)]

mod support;

use bohio::policy::{Grant, Permission};
use support::embedded::{Running, bohio, client_config, id_of, remote_args};

/// A file name holding an escape sequence that sets the clipboard (OSC 52), a bell and a carriage return.
const NAME: &str = "a\x1b]52;c;eA==\x07b\rc";

fn lines(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn a_name_the_host_sends_is_shown_with_its_control_characters_escaped() {
    let dir = tempfile::tempdir().expect("a directory");
    let root = dir.path().join("root");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join(NAME), b"x").unwrap();
    let node = Running::start();
    let config = client_config(dir.path(), "peer");
    let peer = id_of(&config);
    node.keys.accept(peer);
    node.grants.set(
        peer,
        Grant::allowing([Permission::FsRead]).with_fs_roots([root.clone()]),
    );
    let server = node.id.to_string();
    let config = config.to_string_lossy().into_owned();
    let root_text = root.to_string_lossy().into_owned();
    let mut args = remote_args(&node, &config, &server);
    args.extend(["walk", root_text.as_str()]);
    let output = bohio(&args).output().expect("bohio runs");
    let stdout = lines(&output.stdout);
    assert!(output.status.success(), "{}", lines(&output.stderr));
    assert!(
        stdout.contains("a\\x1b]52;c;eA==\\x07b\\x0dc"),
        "{stdout:?}"
    );
    for byte in [0x1b_u8, 0x07, b'\r'] {
        assert!(
            !output.stdout.contains(&byte),
            "a control byte {byte:#x} reached the terminal: {stdout:?}"
        );
    }
}
