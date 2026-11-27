//! Finding the configuration layers and checking what the node would serve from them.
//!
//! [`discover`] reads what `bohio` reads: the layers named, or the system file with the user's merged over it. [`Found::guard`]
//! is the check a node applies before it serves: the layers and the data directory must not be owned or changeable by another
//! local account ([`crate::ownership`] says what is judged). An embedding host that wants the command's behaviour calls both.

use std::path::{Path, PathBuf};

use super::{Config, Environment, Layered, Layers};
use crate::ownership;

/// What the configuration layers named, or found, come to.
#[derive(Debug)]
pub struct Found {
    /// The merged configuration, and the layer files read.
    pub layered: Layered,
    /// The layer files to ask again about the peers: the ones named, or the system file and the user's
    /// whether or not each exists yet (one made later by `bohio peer approve` then applies).
    pub watch: Vec<PathBuf>,
    /// The user file to make when `approve` finds no layer it can write.
    pub create: Option<PathBuf>,
    /// The accounts the operator trusts on the command line or in `BOHIO_TRUST_OWNER`.
    pub trust: Vec<String>,
}

impl Found {
    /// The merged configuration.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.layered.config
    }

    /// Refuses to serve from a layer or a data directory another user may change, unless `[service] check_access` is false:
    /// then what was found is logged and the node serves. A layer or directory another account owns is refused either way,
    /// unless [`Found::trust`] or `[service] trusted_owners` names that account. The data directory is made, as the node
    /// would make it, before it is checked.
    ///
    /// Answers the layers to read the peers from while serving ([`Found::watched`]).
    ///
    /// # Errors
    ///
    /// One line naming every path that is refused and why.
    pub fn guard(&self, environment: &Environment) -> Result<Vec<PathBuf>, String> {
        let config = self.config();
        announce_unchecked(config.service.check_access);
        // The lowest layer cannot vouch for its own owner: only the command line does. A higher layer, and the data
        // directory, are judged by the lowest layer's `trusted_owners` too.
        let mut trusted = self.trust.clone();
        trusted.extend(config.service.trusted_owners.iter().cloned());
        for (index, file) in self.layered.files.iter().enumerate() {
            let who = if index == 0 { &self.trust } else { &trusted };
            ownership::decide(
                &ownership::layer_problems(file),
                config.service.check_access,
                who,
            )?;
        }
        // The layers are the operator's, so the directory they name is made (as the node would make it) and checked.
        let data = config.data_dir(environment);
        make_private_dir(&data);
        let mut problems: Vec<_> = ownership::check(&data, ownership::Guarded::DataDir)
            .err()
            .into_iter()
            .collect();
        let identity = data.join("identity");
        if identity.exists() {
            problems.extend(ownership::check(&identity, ownership::Guarded::Identity).err());
        }
        ownership::decide(&problems, config.service.check_access, &trusted)?;
        Ok(self.watched())
    }

    /// The layers whose peers are read again while serving: each layer read at start, and each that does not exist
    /// yet in a place no other account may change. One another account could make is left out until a restart
    /// checks it, since a layer made while serving is read unchecked.
    #[must_use]
    pub fn watched(&self) -> Vec<PathBuf> {
        let check_access = self.config().service.check_access;
        self.watch
            .iter()
            .filter(|file| {
                if self.layered.files.contains(file) {
                    return true;
                }
                match ownership::may_appear(file) {
                    Ok(()) => true,
                    Err(problem) if read_when_made(problem.kind, check_access) => {
                        tracing::warn!(
                            "{} is read if it is made, as `check_access` is false: {}",
                            file.display(),
                            problem.text
                        );
                        true
                    }
                    Err(problem) => {
                        tracing::info!(
                            "{} is not read until a restart if it is made: {}",
                            file.display(),
                            problem.text
                        );
                        false
                    }
                }
            })
            .cloned()
            .collect()
    }
}

/// Says once, at start, that `[service] check_access` is off.
fn announce_unchecked(check_access: bool) {
    if !check_access {
        tracing::warn!(
            "non-default: [service] check_access is false: who owns and who may write the configuration layers and the data directory is not checked, so any account that can change them decides who connects and what each peer may do (a path another account owns is still refused)"
        );
    }
}

/// Whether a layer that does not exist yet is read when it appears, given why its place is not trusted: the operator's own
/// place that others may change, or that cannot be judged, is waived by `check_access = false`; another account's never.
fn read_when_made(kind: ownership::Kind, check_access: bool) -> bool {
    !check_access && kind != ownership::Kind::Foreign
}

/// Makes the data directory (and what is missing above it) closed to other accounts where the platform has modes.
fn make_private_dir(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path);
    }
    #[cfg(not(unix))]
    let _ = std::fs::create_dir_all(path);
}

/// The configuration file under the system and user directories: `bohio/bohio.cfg`.
fn name() -> PathBuf {
    ["bohio", "bohio.cfg"].iter().collect()
}

/// The system file and the user's by their usual places (the user's is `None` when the account has no home to look in).
#[must_use]
pub fn default_files() -> (PathBuf, Option<PathBuf>) {
    let name = name();
    (
        anycfg::system_config_path(Some(&name)),
        anycfg::user_config_path(Some(&name)).ok(),
    )
}

/// The layers `named` names (alone, lowest first); else the system file with the user's merged over
/// it. `[service] user_layer = false` in the lowest layer leaves the user's file out. Files that do not
/// exist are not layers; none at all is the defaults.
///
/// # Errors
///
/// One line: a layer that cannot be read or parsed, or a system file another account owns or may change.
pub fn discover(named: &[PathBuf]) -> Result<Found, String> {
    let (system, user) = default_files();
    discover_in(named, system, user)
}

/// As [`discover`], with the system file and the user's given.
///
/// # Errors
///
/// As [`discover`].
pub fn discover_in(
    named: &[PathBuf],
    system: PathBuf,
    user: Option<PathBuf>,
) -> Result<Found, String> {
    let named: Vec<PathBuf> = named
        .iter()
        .filter(|path| !path.as_os_str().is_empty())
        .cloned()
        .collect();
    if !named.is_empty() {
        let layered = Layers::load(&named).map_err(|error| error.to_string())?;
        return Ok(Found {
            layered,
            watch: named,
            create: None,
            trust: ownership::trusted_from_environment(),
        });
    }
    let user_layer = if system.is_file() {
        let service = Layers::load(std::slice::from_ref(&system))
            .map_err(|error| error.to_string())?
            .config
            .service;
        // Every command reads the system file, the client's identity and the control endpoint included, so one
        // another account made or may change is refused before anything uses it.
        ownership::decide(
            &ownership::layer_problems(&system),
            service.check_access,
            &ownership::trusted_from_environment(),
        )?;
        service.user_layer
    } else {
        true
    };
    let mut watch = vec![system];
    let mut create = None;
    if user_layer && let Some(user) = user {
        create = Some(user.clone());
        watch.push(user);
    }
    let files: Vec<PathBuf> = watch
        .iter()
        .filter(|path| path.is_file())
        .cloned()
        .collect();
    let layered = Layers::load(&files).map_err(|error| error.to_string())?;
    Ok(Found {
        layered,
        watch,
        create,
        trust: ownership::trusted_from_environment(),
    })
}

#[cfg(test)]
mod tests {
    use super::{announce_unchecked, discover_in, read_when_made};
    use crate::ownership::{Kind, Problem, decide};
    use std::path::PathBuf;

    fn file(dir: &std::path::Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn the_user_file_is_merged_over_the_system_one_unless_the_system_one_turns_it_off() {
        let dir = crate::private_dir::private_tempdir();
        let system = file(dir.path(), "system.cfg", "[service]\nname = \"system\"\n");
        let user = file(dir.path(), "user.cfg", "[service]\nname = \"user\"\n");
        let both = discover_in(&[], system.clone(), Some(user.clone())).unwrap();
        assert_eq!(both.layered.files, [system.clone(), user.clone()]);
        assert_eq!(both.config().service.name, "user");
        assert_eq!(both.create.as_deref(), Some(user.as_path()));

        file(
            dir.path(),
            "system.cfg",
            "[service]\nname = \"system\"\nuser_layer = false\n",
        );
        let off = discover_in(&[], system.clone(), Some(user.clone())).unwrap();
        assert_eq!(off.layered.files, std::slice::from_ref(&system));
        assert_eq!(off.config().service.name, "system");
        assert!(
            off.create.is_none(),
            "a user file nothing reads is not made"
        );
        assert_eq!(off.watch, std::slice::from_ref(&system));

        // Named layers are read alone, whatever the system file says and whether or not they exist.
        let named = discover_in(std::slice::from_ref(&user), system, Some(user.clone())).unwrap();
        assert_eq!(named.layered.files, [user]);
        assert!(named.create.is_none());
    }

    #[test]
    fn a_missing_file_is_not_a_layer_and_no_file_is_the_defaults() {
        let dir = crate::private_dir::private_tempdir();
        let (system, user) = (dir.path().join("none.cfg"), dir.path().join("also.cfg"));
        let found = discover_in(&[], system.clone(), Some(user.clone())).unwrap();
        assert!(found.layered.files.is_empty());
        assert_eq!(found.config(), &crate::config::Config::default());
        assert_eq!(
            found.watch,
            [system, user.clone()],
            "a file made later is watched"
        );
        assert_eq!(found.create, Some(user));
    }

    fn layered_in(dir: &std::path::Path, extra: &str) -> (super::Found, PathBuf) {
        let data = dir.join("data");
        let layer = file(
            dir,
            "node.cfg",
            &format!(
                "[service]
data_dir = '{}'
{extra}",
                data.display()
            ),
        );
        (
            discover_in(std::slice::from_ref(&layer), layer.clone(), None).unwrap(),
            data,
        )
    }

    #[test]
    fn the_guard_accepts_private_layers_makes_the_data_directory_and_answers_the_layers_to_watch() {
        let dir = crate::private_dir::private_tempdir();
        let (found, data) = layered_in(dir.path(), "");
        assert!(!data.exists());
        let watch = found.guard(&crate::config::Environment::default()).unwrap();
        assert!(data.is_dir(), "the node would make it, so the guard does");
        assert_eq!(watch, found.layered.files);
    }

    #[cfg(unix)]
    #[test]
    fn the_guard_refuses_a_layer_others_may_write_unless_the_check_is_turned_off() {
        use std::os::unix::fs::PermissionsExt;
        let environment = crate::config::Environment::default();
        let dir = crate::private_dir::private_tempdir();
        let (found, _) = layered_in(dir.path(), "");
        let layer = found.layered.files[0].clone();
        std::fs::set_permissions(&layer, std::fs::Permissions::from_mode(0o666)).unwrap();
        let refused = found.guard(&environment).unwrap_err();
        assert!(refused.contains("node.cfg"), "{refused}");

        let dir = crate::private_dir::private_tempdir();
        let (found, _) = layered_in(
            dir.path(),
            "check_access = false
",
        );
        let layer = found.layered.files[0].clone();
        std::fs::set_permissions(&layer, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(found.guard(&environment).is_ok());
    }

    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

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

    fn logged(run: impl FnOnce()) -> String {
        let capture = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish();
        tracing::subscriber::with_default(subscriber, run);
        String::from_utf8(capture.0.lock().expect("log").clone()).expect("text")
    }

    #[test]
    fn a_service_check_that_is_off_is_logged_as_non_default_and_one_that_is_on_is_silent() {
        let text = logged(|| announce_unchecked(false));
        assert!(
            text.contains("non-default: [service] check_access is false"),
            "{text}"
        );
        assert!(logged(|| announce_unchecked(true)).is_empty());
    }

    #[test]
    fn a_layer_made_later_in_an_open_place_is_read_only_when_the_check_is_off() {
        assert!(read_when_made(Kind::Open, false));
        assert!(read_when_made(Kind::Unknown, false));
        assert!(!read_when_made(Kind::Open, true));
        assert!(!read_when_made(Kind::Foreign, false));
    }

    fn problem(kind: Kind) -> Problem {
        Problem {
            kind,
            text: "the data directory is open to every user".to_owned(),
            owner: Some("4242".to_owned()),
        }
    }

    #[test]
    fn an_access_problem_is_an_error_until_the_operator_turns_the_check_off() {
        assert!(decide(&[problem(Kind::Open)], true, &[]).is_err());
        assert!(decide(&[problem(Kind::Open)], false, &[]).is_ok());
        assert!(decide(&[problem(Kind::Unknown)], false, &[]).is_ok());
        assert!(decide(&[], true, &[]).is_ok());
    }

    /// Whoever owns a layer wrote what it says, `check_access = false` included, so a layer another account owns
    /// cannot turn its own check off.
    #[test]
    fn a_path_another_account_owns_is_refused_even_when_the_check_is_off() {
        assert!(decide(&[problem(Kind::Foreign)], false, &[]).is_err());
        assert!(decide(&[problem(Kind::Open), problem(Kind::Foreign)], false, &[]).is_err());
    }
}
