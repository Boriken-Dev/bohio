//! The API header names every public module of the crate.

use std::path::Path;

#[test]
fn the_header_lists_every_public_module() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let header = std::fs::read_to_string(root.join("AGENTS.md")).expect("the header");
    let library = std::fs::read_to_string(root.join("src/lib.rs")).expect("the library root");
    let lines: Vec<&str> = library.lines().collect();
    let modules: Vec<&str> = lines
        .iter()
        .enumerate()
        .filter(|(at, _)| *at == 0 || lines[*at - 1] != "#[doc(hidden)]")
        .filter_map(|(_, line)| line.strip_prefix("pub mod "))
        .map(|rest| rest.trim_end_matches([';', '{', ' ']))
        .collect();
    assert!(!modules.is_empty(), "no `pub mod` line was read");
    for module in modules {
        assert!(
            header.contains(&format!("`{module}`")),
            "AGENTS.md does not name the public module `{module}`"
        );
    }
}
