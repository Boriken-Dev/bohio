//! Libraries run in a process of their own (`[providers] isolate`, `isolated`, `library_host`): a library served through a host
//! program, the default of mapping it into the node unchanged, a missing host that is reported and not fatal, and the owner
//! check on the library.
//!
//! The library is `tests/plugin_echo`, built beside this test binary (or in `BOHIO_LIBRARY_DIR`, which is where a static musl
//! test takes the library built for glibc). The host is `guatiao-remote-server`: `BOHIO_TEST_LIBRARY_HOST` names it, else the
//! one beside the test target's `debug` directory, built on first use with `cargo build -p guatiao-source-remote --bin
//! guatiao-remote-server`. A host must match the library: a static musl test takes a glibc one from the variable.

#[path = "support/private_dir.rs"]
mod private_dir;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bohio::access::{LibraryState, ProvidersReport};
use bohio::config::{Config, NodeConfig};
use bohio::node::{MemoryGrants, MemoryKeys, Node, NodeError};
use bohio_api::Terminal;
use bohio_api::guatiao::library::Object;
use bohio_api::model::TerminalOpen;
use cabuya::auth::{Identity, Method};

const WAIT: Duration = Duration::from_secs(30);

fn library() -> PathBuf {
    let exe = std::env::current_exe().expect("a test binary knows its own path");
    let deps = exe.parent().expect("the test binary is in a directory");
    let file = format!(
        "{}bohio_test_plugin_echo{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    );
    let elsewhere = std::env::var_os("BOHIO_LIBRARY_DIR").map(PathBuf::from);
    for dir in elsewhere.into_iter().chain([
        deps.to_path_buf(),
        deps.parent().map(PathBuf::from).unwrap_or_default(),
    ]) {
        let candidate = dir.join(&file);
        if candidate.is_file() {
            return candidate;
        }
    }
    panic!("{file} is not beside {}", deps.display());
}

/// The host program `guatiao-remote-server`.
fn host() -> PathBuf {
    if let Some(named) = std::env::var_os("BOHIO_TEST_LIBRARY_HOST") {
        return PathBuf::from(named);
    }
    let exe = std::env::current_exe().expect("a test binary knows its own path");
    let debug = exe
        .parent()
        .and_then(Path::parent)
        .expect("the test binary is in deps");
    let file = debug.join(format!(
        "guatiao-remote-server{}",
        std::env::consts::EXE_SUFFIX
    ));
    if !file.is_file() {
        let status = std::process::Command::new(env!("CARGO"))
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
            .expect("cargo runs");
        assert!(status.success(), "the host program was not built");
    }
    assert!(file.is_file(), "{} is not built", file.display());
    file
}

fn private_directory() -> tempfile::TempDir {
    private_dir::private_tempdir()
}

/// Copies of the library and the host in a directory only this account may write.
fn staged() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = private_directory();
    let built = self::library();
    let library = dir
        .path()
        .join(built.file_name().expect("a library has a file name"));
    std::fs::copy(&built, &library).expect("the library");
    let host = dir
        .path()
        .join(format!("host{}", std::env::consts::EXE_SUFFIX));
    std::fs::copy(self::host(), &host).expect("the host");
    (dir, library, host)
}

fn quoted(path: &Path) -> String {
    format!("'{}'", path.display())
}

/// `[providers]` text, and the source that names the library's provider.
fn config(providers: &str) -> NodeConfig {
    let text = format!(
        "[providers]\n{providers}\n[source.echo]\nprovider = \"library-echo\"\n[access]\nterminal = [\"echo\"]\ndesktop = []\n"
    );
    Config::from_toml(&text, "bohio.cfg")
        .expect("a valid configuration")
        .node()
}

fn build(config: NodeConfig) -> Result<Node, NodeError> {
    Node::new(
        config,
        Identity::generate(Method::DEFAULT).expect("an identity"),
        Arc::new(MemoryKeys::new()),
        Arc::new(MemoryGrants::new()),
    )
}

fn report(node: &Node) -> ProvidersReport {
    node.sources().libraries()
}

fn console(node: &Node) {
    let session: Object<dyn Terminal> = node
        .sources()
        .terminal(Some("echo"))
        .expect("the echo source")
        .get()
        .open(TerminalOpen::default())
        .expect("a session opens");
    let mut got = Vec::new();
    let mut buffer = [0_u8; 256];
    session.write(b"hi\r\n").expect("typing");
    let until = Instant::now() + WAIT;
    while !got.windows(16).any(|window| window == b"library-echo: hi") {
        assert!(Instant::now() < until, "nothing came back: {got:?}");
        let count = session.read(&mut buffer, 100).expect("a read");
        if let Ok(count) = usize::try_from(count) {
            got.extend_from_slice(&buffer[..count]);
        }
    }
}

#[test]
fn a_library_served_by_a_host_gives_a_console() {
    let (_dir, library, host) = staged();
    let node = build(config(&format!(
        "paths = [{}]\nisolate = true\nlibrary_host = {}",
        quoted(&library),
        quoted(&host)
    )))
    .expect("the node starts");
    let report = report(&node);
    assert_eq!(report.libraries.len(), 1, "{report:?}");
    let entry = &report.libraries[0];
    assert_eq!(entry.state, LibraryState::Loaded, "{entry:?}");
    assert!(
        entry.detail.contains("in a process of its own") && entry.detail.contains("library-echo"),
        "{}",
        entry.detail
    );
    assert!(entry.digest.is_some());
    assert!(
        report.programs.is_empty(),
        "a hosted library is not a program entry"
    );
    console(&node);
}

#[test]
fn isolated_names_one_library_and_the_rest_stays_in_the_process() {
    let (_dir, library, host) = staged();
    let node = build(config(&format!(
        "isolated = [{}]\nlibrary_host = {}",
        quoted(&library),
        quoted(&host)
    )))
    .expect("the node starts");
    assert_eq!(
        report(&node).libraries[0].state,
        LibraryState::Loaded,
        "{:?}",
        report(&node)
    );
    console(&node);
}

#[test]
fn without_isolate_the_library_is_mapped_into_the_node_as_before() {
    let (_dir, library, _host) = staged();
    let node = build(config(&format!("paths = [{}]", quoted(&library)))).expect("the node starts");
    let report = report(&node);
    let entry = &report.libraries[0];
    assert_eq!(entry.state, LibraryState::Loaded, "{entry:?}");
    assert!(
        !entry.detail.contains("process of its own"),
        "{}",
        entry.detail
    );
    console(&node);
}

#[test]
fn a_missing_host_is_reported_and_the_node_starts() {
    let (_dir, library, _host) = staged();
    let missing = library.with_file_name("no-such-host");
    // Named and absent.
    let node = build(NodeConfig {
        source: Default::default(),
        access: Default::default(),
        ..config(&format!(
            "paths = [{}]\nisolate = true\nlibrary_host = {}",
            quoted(&library),
            quoted(&missing)
        ))
    })
    .expect("the node starts");
    let entry = &report(&node).libraries[0];
    assert_eq!(entry.state, LibraryState::Failed, "{entry:?}");
    assert!(entry.detail.contains("names no file"), "{}", entry.detail);
    // Not named, and none beside the node.
    let node = build(NodeConfig {
        source: Default::default(),
        access: Default::default(),
        ..config(&format!("paths = [{}]\nisolate = true", quoted(&library)))
    })
    .expect("the node starts");
    let entry = &report(&node).libraries[0];
    assert_eq!(entry.state, LibraryState::Failed, "{entry:?}");
    assert!(entry.detail.contains("no library host"), "{}", entry.detail);
}

#[test]
fn a_library_another_account_can_write_is_refused_even_when_it_runs_apart() {
    let (dir, library, host) = staged();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o777))
            .expect("open to everyone");
        let text = format!(
            "paths = [{}]\nisolate = true\nlibrary_host = {}",
            quoted(&library),
            quoted(&host)
        );
        let node = build(NodeConfig {
            source: Default::default(),
            access: Default::default(),
            ..config(&text)
        })
        .expect("the node starts");
        let entry = &report(&node).libraries[0];
        assert_eq!(entry.state, LibraryState::Failed, "{entry:?}");
        assert!(entry.detail.starts_with("refused:"), "{}", entry.detail);
    }
    #[cfg(not(unix))]
    {
        let _ = (dir, library, host);
    }
}

#[test]
fn the_settings_default_to_the_ordinary_load_and_are_checked() {
    let config = Config::from_toml("", "bohio.cfg").expect("an empty configuration");
    assert!(!config.providers.isolate);
    assert!(config.providers.isolated.is_empty());
    assert!(config.providers.library_host.is_none());
    for text in [
        "[providers]\nlibrary_host = \"\"\n",
        "[providers]\nisolated = [\"\"]\n",
    ] {
        let result = Config::from_toml(text, "bohio.cfg")
            .map_err(|error| error.to_string())
            .and_then(|config| {
                build(config.node())
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            });
        let Err(error) = result else {
            panic!("{text} cannot work");
        };
        assert!(error.contains("providers."), "{text}: {error}");
    }
}
