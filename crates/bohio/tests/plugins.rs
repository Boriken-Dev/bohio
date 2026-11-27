//! Provider libraries loaded at run time (`[providers] paths`): what loads, what is passed over and why, what one broken
//! library does to the others, and the owner check with its opt-out.
//!
//! The libraries are built beside this test binary (`bohio-provider-serial-plugin`, the serial provider as a library, and
//! `bohio-test-plugin-other`, a library of a kind that is not Bohío's), copied into a directory the test makes and loaded from
//! there. A node built with feature `serial` already has the `serial` provider, which a library cannot replace: the console test
//! therefore runs in a build without that feature (`--no-default-features --features cli`).

#[path = "support/private_dir.rs"]
mod private_dir;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bohio::access::{LibraryState, ProvidersReport};
use bohio::config::{Config, DlopenChoice, NodeConfig, ProvidersConfig};
use bohio::node::{MemoryGrants, MemoryKeys, Node, NodeError};
use cabuya::auth::{Identity, Method};
use sha2::{Digest, Sha256};

/// Where the library is: in `BOHIO_LIBRARY_DIR` when that names it (a static musl test binary loads the libraries built for
/// glibc, since such a target cannot build a `cdylib`), else where cargo put a dev-dependency cdylib: beside this binary, or
/// one up.
fn built(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("a test binary knows its own path");
    let deps = exe.parent().expect("the test binary is in a directory");
    let file = format!(
        "{}{name}{}",
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

fn serial_plugin() -> PathBuf {
    built("bohio_provider_serial_plugin")
}

fn ssh_plugin() -> PathBuf {
    built("bohio_provider_ssh_plugin")
}

fn other_plugin() -> PathBuf {
    built("bohio_test_plugin_other")
}

/// A copy of `from` named `name` in `dir`.
fn put(dir: &Path, name: &str, from: &Path) -> PathBuf {
    let extension = from.extension().expect("a library extension");
    let to = dir.join(name).with_extension(extension);
    std::fs::copy(from, &to).expect("the copy");
    to
}

fn providers(paths: &[&Path], check_access: bool) -> ProvidersConfig {
    ProvidersConfig {
        paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        check_access,
        ..ProvidersConfig::default()
    }
}

fn build(config: NodeConfig) -> Result<Node, NodeError> {
    Node::new(
        config,
        Identity::generate(Method::DEFAULT).expect("an identity"),
        Arc::new(MemoryKeys::new()),
        Arc::new(MemoryGrants::new()),
    )
}

fn started(providers: ProvidersConfig) -> ProvidersReport {
    let node = build(NodeConfig {
        providers,
        ..NodeConfig::default()
    })
    .expect("the node starts");
    node.sources().libraries()
}

fn entry<'a>(report: &'a ProvidersReport, file: &Path) -> &'a bohio::access::Library {
    report
        .libraries
        .iter()
        .find(|library| library.path == file)
        .unwrap_or_else(|| panic!("{} is not in {report:?}", file.display()))
}

/// The directory a test loads from: owned by this account and writable by no one else.
fn private_directory() -> tempfile::TempDir {
    private_dir::private_tempdir()
}

#[test]
fn nothing_loads_and_nothing_runs_unless_paths_are_set() {
    let report = started(ProvidersConfig::default());
    assert!(report.libraries.is_empty());
    assert!(report.check_access, "the check is on by default");
    let config = Config::from_toml("", "bohio.cfg").expect("an empty configuration");
    assert!(config.providers.paths.is_empty());
    assert!(config.providers.check_access);
}

#[test]
fn the_serial_library_loads_from_a_path_and_is_reported_with_its_digest() {
    let dir = private_directory();
    let file = put(dir.path(), "serial", &serial_plugin());
    let report = started(providers(&[&file], true));
    let library = entry(&report, &file);
    assert_eq!(library.state, LibraryState::Loaded, "{library:?}");
    assert!(
        library.detail.contains("bohio-provider-serial-plugin"),
        "{}",
        library.detail
    );
    let want = format!(
        "sha256:{}",
        Sha256::digest(std::fs::read(&file).expect("the file"))
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    );
    assert_eq!(library.digest.as_deref(), Some(want.as_str()));
    if cfg!(feature = "serial") {
        // The node has `serial` built in, and a library never replaces a built-in provider.
        assert!(
            library.detail.contains("already registered"),
            "{}",
            library.detail
        );
    } else {
        assert!(library.detail.contains("1 provider"), "{}", library.detail);
    }
}

#[test]
fn the_ssh_library_loads_and_offers_the_ssh_provider() {
    let dir = private_directory();
    let file = put(dir.path(), "ssh", &ssh_plugin());
    let report = started(providers(&[&file], true));
    let library = entry(&report, &file);
    assert_eq!(library.state, LibraryState::Loaded, "{library:?}");
    assert!(
        library.detail.contains("bohio-provider-ssh-plugin"),
        "{}",
        library.detail
    );
    if cfg!(feature = "ssh") {
        // The node has `ssh` built in, and a library never replaces a built-in provider.
        assert!(
            library.detail.contains("already registered"),
            "{}",
            library.detail
        );
    } else {
        assert!(library.detail.contains("1 provider"), "{}", library.detail);
    }
}

#[test]
fn a_directory_loads_each_library_and_a_stale_copy_is_reported_not_doubled() {
    let dir = private_directory();
    let new = put(dir.path(), "a_serial", &serial_plugin());
    let stale = put(dir.path(), "b_serial_old", &serial_plugin());
    std::fs::write(dir.path().join("notes.txt"), b"not a library").expect("a note");
    let report = started(providers(&[dir.path()], true));
    assert_eq!(report.libraries.len(), 2, "{report:?}");
    assert_eq!(entry(&report, &new).state, LibraryState::Loaded);
    let second = entry(&report, &stale);
    assert_eq!(second.state, LibraryState::Skipped, "{second:?}");
    assert!(
        second.detail.contains("already loaded"),
        "{}",
        second.detail
    );
}

#[test]
fn a_library_that_declares_no_bohio_kind_is_not_mapped_and_the_report_says_why() {
    let dir = private_directory();
    let other = put(dir.path(), "other", &other_plugin());
    let serial = put(dir.path(), "serial", &serial_plugin());
    let report = started(providers(&[dir.path()], true));
    let library = entry(&report, &other);
    assert_eq!(library.state, LibraryState::Skipped, "{library:?}");
    assert!(
        library.detail.contains("declares no Bohío kind")
            && library.detail.contains("other_greeter"),
        "{}",
        library.detail
    );
    assert!(library.digest.is_some());
    assert_eq!(entry(&report, &serial).state, LibraryState::Loaded);
}

#[test]
fn a_broken_library_or_path_fails_alone_while_the_others_load() {
    let dir = private_directory();
    let good = put(dir.path(), "b_good", &serial_plugin());
    // The head of a library whose tail is missing: it names the entry symbol and cannot be mapped.
    let bytes = std::fs::read(serial_plugin()).expect("the library");
    let plugin = serial_plugin();
    let broken = dir
        .path()
        .join("a_broken")
        .with_extension(plugin.extension().expect("a library extension"));
    std::fs::write(&broken, &bytes[..bytes.len() * 3 / 4]).expect("a truncated copy");
    let missing = dir.path().join("nothing_here");
    let report = started(providers(&[&missing, dir.path()], true));
    assert_eq!(entry(&report, &missing).state, LibraryState::Failed);
    assert_ne!(entry(&report, &broken).state, LibraryState::Loaded);
    assert_eq!(entry(&report, &good).state, LibraryState::Loaded);
}

mod owner {
    use super::*;
    use std::sync::Mutex;

    #[cfg(unix)]
    fn open_to_everyone(dir: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o777))
            .expect("open to everyone");
    }

    /// Everyone (`S-1-1-0`) may modify the directory and what is made in it.
    #[cfg(windows)]
    fn open_to_everyone(dir: &Path) {
        let status = std::process::Command::new("icacls")
            .arg(dir)
            .args(["/grant", "*S-1-1-0:(OI)(CI)M"])
            .stdout(std::process::Stdio::null())
            .status()
            .expect("icacls runs");
        assert!(status.success(), "icacls granted Everyone");
    }

    fn open_directory() -> (tempfile::TempDir, PathBuf) {
        let dir = private_directory();
        let file = put(dir.path(), "serial", &serial_plugin());
        open_to_everyone(dir.path());
        (dir, file)
    }

    #[test]
    fn a_library_from_a_directory_another_account_can_write_is_refused_by_default() {
        let (_dir, file) = open_directory();
        let report = started(providers(&[&file], true));
        let library = entry(&report, &file);
        assert_eq!(library.state, LibraryState::Failed, "{library:?}");
        assert!(library.detail.starts_with("refused:"), "{}", library.detail);
        assert!(library.digest.is_none(), "a refused file is not read");
    }

    #[test]
    fn with_the_check_off_it_loads_and_the_report_and_the_log_say_so() {
        let (_dir, file) = open_directory();
        let log = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_writer(Capture(Arc::clone(&log)))
            .with_ansi(false)
            .finish();
        let report =
            tracing::subscriber::with_default(subscriber, || started(providers(&[&file], false)));
        let library = entry(&report, &file);
        assert_eq!(library.state, LibraryState::Loaded, "{library:?}");
        assert!(!report.check_access);
        let text = String::from_utf8(log.lock().expect("log").clone()).expect("text");
        assert!(
            text.contains("non-default: [providers] check_access is false"),
            "{text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_library_file_another_account_can_write_is_refused_whatever_the_directory() {
        let dir = private_directory();
        let file = put(dir.path(), "serial", &serial_plugin());
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o666))
            .expect("open to everyone");
        let report = started(providers(&[&file], true));
        assert_eq!(entry(&report, &file).state, LibraryState::Failed);
    }

    #[cfg(unix)]
    #[test]
    fn an_ld_so_in_a_directory_another_account_can_write_is_refused_and_no_library_is_mapped() {
        let libraries = private_directory();
        let file = put(libraries.path(), "serial", &serial_plugin());
        let open = private_directory();
        let ld_so = open.path().join("ld.so");
        std::fs::write(&ld_so, b"not a linker").expect("a file");
        open_to_everyone(open.path());
        let mut config = providers(&[&file], true);
        config.ld_so = Some(ld_so.clone());
        let report = started(config);
        let refused = entry(&report, &ld_so);
        assert_eq!(refused.state, LibraryState::Failed, "{refused:?}");
        assert!(
            refused.detail.contains("ld_so") && refused.detail.contains("refused"),
            "{}",
            refused.detail
        );
        assert!(
            report.libraries.iter().all(|library| library.path != file),
            "nothing is mapped through a linker that is not trusted: {report:?}"
        );

        // With the check off the linker is not looked at, and the library loads as it always did.
        let mut config = providers(&[&file], false);
        config.ld_so = Some(ld_so);
        let report = started(config);
        assert_eq!(entry(&report, &file).state, LibraryState::Loaded);
    }

    /// Collects what the node logs.
    #[derive(Clone)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log").extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
        type Writer = Self;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }
}

#[test]
fn an_empty_path_and_a_layer_above_the_lowest_weakening_the_check_are_refused() {
    let config = NodeConfig {
        providers: ProvidersConfig {
            paths: vec![PathBuf::new()],
            check_access: true,
            ..ProvidersConfig::default()
        },
        ..NodeConfig::default()
    };
    let Err(error) = build(config) else {
        panic!("an empty path cannot name a library");
    };
    assert!(error.to_string().contains("providers.paths"), "{error}");

    let dir = tempfile::tempdir().expect("a directory");
    let lowest = dir.path().join("a.cfg");
    let above = dir.path().join("b.cfg");
    std::fs::write(&lowest, "").expect("a layer");
    std::fs::write(&above, "[providers]\ncheck_access = false\n").expect("a layer");
    // The key is ignored (and logged), not read: the check stays on.
    let layered = bohio::config::Layers::load(&[lowest, above]).expect("the layers load");
    assert!(
        layered.config.providers.check_access,
        "a higher layer may not weaken the check"
    );
}

#[test]
fn a_source_naming_a_provider_no_library_offers_is_refused_when_the_node_is_built() {
    let dir = private_directory();
    let text = format!(
        "[providers]\npaths = [\"{}\"]\n[source.console]\nprovider = \"nothing_offers_this\"\n",
        dir.path().display().to_string().replace('\\', "/")
    );
    // Reading the file maps nothing, so it cannot tell; building the node, which maps the libraries first, can.
    let config = Config::from_toml(&text, "bohio.cfg").expect("the file reads");
    let Err(error) = build(config.node()) else {
        panic!("no provider of that name exists");
    };
    assert!(
        error.to_string().contains("source.console.provider"),
        "{error}"
    );
}

#[test]
fn the_loader_settings_default_to_the_strict_choice_and_only_the_lowest_layer_names_code() {
    let config = Config::from_toml("", "bohio.cfg").expect("an empty configuration");
    assert_eq!(config.providers.dlopen, DlopenChoice::Auto);
    assert!(config.providers.ld_so.is_none());
    assert!(!config.providers.allow_reserved_signal_handlers);
    let config = Config::from_toml(
        "[providers]\ndlopen = \"foreign\"\nld_so = \"/opt/glibc/ld.so\"\nallow_reserved_signal_handlers = true\n",
        "bohio.cfg",
    )
    .expect("the keys read");
    assert_eq!(config.providers.dlopen, DlopenChoice::Foreign);
    assert_eq!(
        config.providers.ld_so.as_deref(),
        Some(Path::new("/opt/glibc/ld.so"))
    );
    assert!(config.providers.allow_reserved_signal_handlers);
    let Err(error) = Config::from_toml("[providers]\ndlopen = \"magic\"\n", "bohio.cfg") else {
        panic!("a loader nobody has is refused");
    };
    assert!(error.to_string().contains("dlopen"), "{error}");
    let empty = ProvidersConfig {
        ld_so: Some(PathBuf::new()),
        ..ProvidersConfig::default()
    };
    let Err(error) = build(NodeConfig {
        providers: empty,
        ..NodeConfig::default()
    }) else {
        panic!("an empty linker path cannot work");
    };
    assert!(error.to_string().contains("providers.ld_so"), "{error}");

    let dir = tempfile::tempdir().expect("a directory");
    let lowest = dir.path().join("a.cfg");
    std::fs::write(&lowest, "").expect("a layer");
    for key in [
        "ld_so = \"/opt/glibc/ld.so\"",
        "allow_reserved_signal_handlers = true",
    ] {
        let above = dir.path().join("b.cfg");
        std::fs::write(&above, format!("[providers]\n{key}\n")).expect("a layer");
        let layered = bohio::config::Layers::load(&[lowest.clone(), above])
            .expect("the layers load: the key is ignored");
        assert!(
            layered.config.providers.ld_so.is_none()
                && !layered.config.providers.allow_reserved_signal_handlers,
            "a higher layer may not set {key}"
        );
    }
}

/// A statically linked Linux node maps a library built for glibc through the system's own `ld.so`, and `dlopen = "system"`
/// turns that off.
#[cfg(all(
    target_os = "linux",
    target_feature = "crt-static",
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
#[test]
fn a_static_build_maps_a_glibc_library_and_the_platform_loader_can_be_chosen() {
    let dir = private_directory();
    let file = put(dir.path(), "serial", &serial_plugin());

    let mut platform = providers(&[&file], true);
    platform.dlopen = DlopenChoice::System;
    let report = started(platform);
    let library = entry(&report, &file);
    assert_eq!(library.state, LibraryState::Failed, "{library:?}");
    assert!(
        library.detail.contains("isolate = true"),
        "the report names the way out: {}",
        library.detail
    );

    let report = started(providers(&[&file], true));
    let library = entry(&report, &file);
    assert_eq!(library.state, LibraryState::Loaded, "{library:?}");
}

#[cfg(all(unix, not(feature = "serial")))]
mod console {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::process::{Child, Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use bohio::node::PeerLink;
    use bohio::policy::{Grant, Permission};
    use bohio_client::link::{Settings, stream};
    use bohio_client::shell::Shell;
    use bohio_proto::shell::Request;
    use coqui_transport::{Channel, memory};

    const WAIT: Duration = Duration::from_secs(30);

    /// `socat` joining two pseudo-terminals as a null-modem cable; `None` when it is not installed.
    struct Cable {
        child: Child,
        near: String,
        far: String,
    }

    impl Cable {
        fn make() -> Option<Self> {
            let mut child = Command::new("socat")
                .args(["-d", "-d", "pty,raw,echo=0", "pty,raw,echo=0"])
                .stderr(Stdio::piped())
                .spawn()
                .ok()?;
            let stderr = child.stderr.take()?;
            let mut paths = Vec::new();
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if let Some(at) = line.find("PTY is ") {
                    paths.push(line[at + 7..].trim().to_owned());
                }
                if paths.len() == 2 {
                    break;
                }
            }
            let far = paths.pop()?;
            let near = paths.pop()?;
            Some(Self { child, near, far })
        }
    }

    impl Drop for Cable {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[test]
    fn the_serial_library_serves_a_console_over_a_serial_cable() {
        let Some(cable) = Cable::make() else {
            println!("SKIPPED: socat is not installed");
            return;
        };
        let dir = private_directory();
        let file = put(dir.path(), "serial", &serial_plugin());
        let config: Config = Config::from_toml(
            &format!(
                "[providers]\npaths = [\"{}\"]\n[source.console]\nprovider = \"serial\"\npath = \"{}\"\nreopen_after = \"200ms\"\n[access]\nterminal = [\"console\"]\ndesktop = []\n",
                file.display(),
                cable.near
            ),
            "bohio.cfg",
        )
        .expect("a valid configuration");
        let host_identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let host_id = host_identity.peer_id();
        let client_identity = Identity::generate(Method::DEFAULT).expect("an identity");
        let client_id = client_identity.peer_id();
        let (host_keys, client_keys) = (Arc::new(MemoryKeys::new()), Arc::new(MemoryKeys::new()));
        let host_grants = Arc::new(MemoryGrants::new());
        host_keys.accept(client_id);
        host_grants.set(
            client_id,
            Grant::allowing([Permission::Info, Permission::Shell, Permission::Session]),
        );
        client_keys.accept(host_id);
        let host = Arc::new(
            Node::new(config.node(), host_identity, host_keys, host_grants)
                .expect("the host starts")
                .with_path(std::env::var_os("PATH").unwrap_or_default()),
        );
        let library = host.sources().libraries();
        assert_eq!(library.libraries.len(), 1, "{library:?}");
        assert_eq!(library.libraries[0].state, LibraryState::Loaded);
        let client = Arc::new(
            Node::new(
                NodeConfig::default(),
                client_identity,
                client_keys,
                Arc::new(MemoryGrants::new()),
            )
            .expect("the client starts"),
        );
        let settings = Settings::default();
        let (one, two) = memory::pipe(1024 * 1024);
        let dial_end = Arc::new(stream(one, &settings).expect("a channel"));
        let answer_end = Arc::new(stream(two, &settings).expect("a channel"));
        let threads = [
            thread::spawn({
                let (node, channel) = (client.clone(), dial_end.clone());
                move || {
                    let _ = node.connect_channel(&*channel, &host_id, None);
                    channel.close();
                }
            }),
            thread::spawn({
                let (node, channel) = (host.clone(), answer_end.clone());
                move || {
                    node.serve_channel(&*channel, None);
                    channel.close();
                }
            }),
        ];
        let until = Instant::now() + WAIT;
        let link: PeerLink = loop {
            if let Some(link) = client.peer(&host_id) {
                break link;
            }
            assert!(Instant::now() < until, "no link came up");
            thread::sleep(Duration::from_millis(20));
        };
        let mut far = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&cable.far)
            .expect("the far end opens");
        let shell =
            Shell::start(&link, &Request::console(None), Some(WAIT)).expect("the console opens");
        far.write_all(b"login: ").expect("the far end writes");
        shell
            .read_until(b"login: ", Some(WAIT))
            .expect("the library's console shows what the far end wrote");
        shell.write(b"root\r").expect("typing");
        let mut got = Vec::new();
        let mut byte = [0_u8; 1];
        let until = Instant::now() + WAIT;
        while !got.ends_with(b"root\r") && Instant::now() < until {
            if far.read(&mut byte).unwrap_or(0) == 1 {
                got.push(byte[0]);
            }
        }
        assert!(got.ends_with(b"root\r"), "the far end read {got:?}");
        drop(shell);
        dial_end.close();
        answer_end.close();
        for handle in threads {
            let _ = handle.join();
        }
    }
}
