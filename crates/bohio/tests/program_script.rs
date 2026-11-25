//! A provider program in another language: `examples/programs/power_rhai`, a Bohío power source written in Rhai, served by the
//! stock `guatiao-remote-server` with guatiao's Rhai source, started by a node with `[[providers.program]]` and used through
//! the node's power source.
//!
//! Needs guatiao's Rhai source file (`guatiao-source-rhai`, a workspace of its own that builds Rhai and libffi into one file) and
//! the host `guatiao-remote-server`. `BOHIO_TEST_RHAI_SOURCE` and `BOHIO_TEST_LIBRARY_HOST` name them; without them the test
//! builds them with cargo (the Rhai source from the guatiao checkout this workspace builds against, into `rhai` beside the test
//! target) and prints `SKIPPED` with the reason when it cannot (no network for the Rhai crates, no C compiler for libffi).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use bohio::config::Config;
use bohio::node::{MemoryGrants, MemoryKeys, Node};
use cabuya::auth::{Identity, Method};

fn debug_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("a test binary knows its own path");
    exe.parent()
        .and_then(Path::parent)
        .expect("the test binary is in deps")
        .to_path_buf()
}

/// The host program `guatiao-remote-server`.
fn host() -> Result<PathBuf, String> {
    if let Some(named) = std::env::var_os("BOHIO_TEST_LIBRARY_HOST") {
        return Ok(PathBuf::from(named));
    }
    let debug = debug_dir();
    let file = debug.join(format!(
        "guatiao-remote-server{}",
        std::env::consts::EXE_SUFFIX
    ));
    if !file.is_file() {
        let status = Command::new(env!("CARGO"))
            .args([
                "build",
                "-p",
                "guatiao-source-remote",
                "--bin",
                "guatiao-remote-server",
            ])
            .arg("--target-dir")
            .arg(debug.parent().expect("a target directory"))
            .status()
            .map_err(|error| format!("cargo does not run: {error}"))?;
        if !status.success() || !file.is_file() {
            return Err("the host program could not be built".to_owned());
        }
    }
    Ok(file)
}

/// guatiao's Rhai source file.
fn rhai_source() -> Result<PathBuf, String> {
    if let Some(named) = std::env::var_os("BOHIO_TEST_RHAI_SOURCE") {
        return Ok(PathBuf::from(named));
    }
    let metadata = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--locked"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .map_err(|error| format!("cargo metadata does not run: {error}"))?;
    // JSON writes a Windows path with doubled backslashes; forward slashes name the same file.
    let text = String::from_utf8_lossy(&metadata.stdout).replace("\\\\", "/");
    // The manifest of guatiao-source-remote names the checkout; the Rhai source is its sibling.
    let marker = "guatiao-source-remote/Cargo.toml";
    let end = text
        .find(marker)
        .ok_or("cargo metadata does not name guatiao-source-remote")?;
    let start = text[..end]
        .rfind('"')
        .ok_or("cargo metadata is not what it was")?
        + 1;
    let remote = PathBuf::from(&text[start..end + marker.len()]);
    let manifest = remote
        .parent()
        .and_then(Path::parent)
        .map(|crates| crates.join("guatiao-source-rhai").join("Cargo.toml"))
        .ok_or("the checkout has no guatiao-source-rhai")?;
    let target = debug_dir()
        .parent()
        .expect("a target directory")
        .join("rhai");
    let status = Command::new(env!("CARGO"))
        .arg("build")
        .arg("--manifest-path")
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&target)
        .status()
        .map_err(|error| format!("cargo does not run: {error}"))?;
    let file = target.join("debug").join(format!(
        "{}guatiao_source_rhai{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    if status.success() && file.is_file() {
        Ok(file)
    } else {
        Err(format!("{} could not be built", manifest.display()))
    }
}

fn private_directory() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("a directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
            .expect("private");
    }
    dir
}

/// A path as JSON and TOML literal text may hold it: forward slashes.
fn slashed(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

#[test]
fn a_power_source_written_in_rhai_is_served_to_a_node_by_a_program() {
    let (host, rhai) = match (host(), rhai_source()) {
        (Ok(host), Ok(rhai)) => (host, rhai),
        (host, rhai) => {
            println!(
                "SKIPPED: {}",
                host.err().or(rhai.err()).expect("one of them failed")
            );
            return;
        }
    };
    let dir = private_directory();
    let host_copy = dir
        .path()
        .join(format!("host{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(&host, &host_copy).expect("the host");
    let scripts = dir.path().join("scripts");
    std::fs::create_dir(&scripts).expect("a directory");
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/programs/power_rhai");
    for name in ["rhai_power.rhai", "rhai_power.guatiao"] {
        std::fs::copy(examples.join(name), scripts.join(name)).expect("an example file");
    }
    let kinds = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../bohio-api/bohio-api.kinds.json")
        .canonicalize()
        .expect("the kinds document");
    let source = format!(
        "{}={{\"dirs\":[\"{}\"],\"kinds\":[\"{}\"]}}",
        slashed(&rhai),
        slashed(&scripts),
        slashed(&kinds)
    );
    let text = format!(
        "[[providers.program]]\ncommand = ['{}', '--stdio', '--kinds', '{}', '--source', '{}']\n\
         [source.switch]\nprovider = \"rhai_power_power_switch\"\n[access]\npower = [\"switch\"]\n",
        slashed(&host_copy),
        slashed(&kinds),
        source
    );
    let node = Node::new(
        Config::from_toml(&text, "bohio.cfg").expect("valid").node(),
        Identity::generate(Method::DEFAULT).expect("an identity"),
        Arc::new(MemoryKeys::new()),
        Arc::new(MemoryGrants::new()),
    )
    .expect("the node starts");
    let report = node.sources().libraries();
    assert_eq!(report.programs.len(), 1, "{report:?}");
    println!("{:?}", report.programs[0]);

    let power = node.sources().power(Some("switch")).expect("the source");
    let info = power.get().describe().expect("it describes itself");
    assert_eq!(info.provider, "rhai-power");
    assert!(info.available);
    assert_eq!(power.get().state().expect("a state").state, "on");
    assert_eq!(power.get().act("off").expect("off").state, "off");
    assert_eq!(power.get().state().expect("a state").state, "off");
    assert!(
        power.get().act("reset").is_err(),
        "an action the script refuses is refused"
    );
    assert_eq!(power.get().state().expect("a state").state, "off");
}
