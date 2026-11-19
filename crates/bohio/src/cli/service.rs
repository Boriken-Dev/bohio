//! `bohio service install|uninstall|run`: the host as a Windows service.
//!
//! The arguments and the command line a service is registered with are plain data and are built on
//! every platform; only the calls into the service manager are Windows-only.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{Args, Subcommand};

use crate::config::Environment;

/// The name a service is registered under when `--name` is not given.
pub const DEFAULT_NAME: &str = "bohio";

/// The account a service runs as when `--account` is not given: a built-in account with no
/// administrative rights.
pub const DEFAULT_ACCOUNT: &str = r"NT AUTHORITY\LocalService";

/// `bohio service`.
#[derive(Debug, Args)]
pub struct ServiceArgs {
    /// What to do.
    #[command(subcommand)]
    pub action: ServiceAction,
}

/// The `bohio service` actions.
#[derive(Debug, Subcommand)]
pub enum ServiceAction {
    /// Register this program as a Windows service that runs `bohio service run` (needs an elevated prompt).
    Install(InstallArgs),
    /// Stop and remove the service (needs an elevated prompt).
    Uninstall(NameArgs),
    /// Run as the service: started by the service manager, not by a person.
    Run(RunArgs),
}

/// `bohio service install`.
#[derive(Debug, Args)]
pub struct InstallArgs {
    /// The configuration file the service reads; an absolute path is stored. Without it the service
    /// searches the system location, `%ProgramData%\bohio\bohio.cfg`.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// The service's name.
    #[arg(long, value_name = "NAME", default_value = DEFAULT_NAME)]
    pub name: String,
    /// The account the service runs as. That account must be able to write the data directory.
    #[arg(long, value_name = "ACCOUNT", default_value = DEFAULT_ACCOUNT)]
    pub account: String,
}

/// `bohio service uninstall`.
#[derive(Debug, Args)]
pub struct NameArgs {
    /// The service's name.
    #[arg(long, value_name = "NAME", default_value = DEFAULT_NAME)]
    pub name: String,
}

/// `bohio service run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// The configuration file; otherwise `BOHIO_CONFIG`, then the system's.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// The name the service was registered under.
    #[arg(long, value_name = "NAME", default_value = DEFAULT_NAME)]
    pub name: String,
}

/// What the service manager is given at install: the arguments that start the service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registration {
    /// The service's name.
    pub name: String,
    /// The account it runs as.
    pub account: String,
    /// The arguments after the program: `service run --name NAME [--config FILE]`.
    pub launch_arguments: Vec<OsString>,
}

/// Builds the registration for `args`. `config` is made absolute, because the service manager
/// starts the program in `System32`.
///
/// # Errors
///
/// A name that is empty or holds a path separator, an empty account, or a configuration path that
/// cannot be made absolute.
pub fn registration(args: &InstallArgs) -> Result<Registration, String> {
    check_name(&args.name)?;
    if args.account.trim().is_empty() {
        return Err("the account is empty".to_owned());
    }
    let mut launch_arguments: Vec<OsString> = vec![
        "service".into(),
        "run".into(),
        "--name".into(),
        args.name.clone().into(),
    ];
    if let Some(config) = &args.config {
        let absolute = std::path::absolute(config)
            .map_err(|error| format!("cannot resolve {}: {error}", config.display()))?;
        launch_arguments.push("--config".into());
        launch_arguments.push(absolute.into_os_string());
    }
    Ok(Registration {
        name: args.name.clone(),
        account: args.account.clone(),
        launch_arguments,
    })
}

/// A service name is what the manager keys on: not empty, and no `/` or `\`.
///
/// # Errors
///
/// The reason a name is refused.
pub fn check_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("the service name is empty".to_owned());
    }
    if name.contains(['/', '\\']) {
        return Err("a service name holds no `/` or `\\`".to_owned());
    }
    Ok(())
}

/// Runs a `bohio service` action.
pub(super) fn run(args: &ServiceArgs, environment: &Environment) -> Result<(), String> {
    match &args.action {
        ServiceAction::Install(install) => {
            let registration = registration(install)?;
            platform::install(&registration)
        }
        ServiceAction::Uninstall(uninstall) => {
            check_name(&uninstall.name)?;
            platform::uninstall(&uninstall.name)
        }
        ServiceAction::Run(run) => {
            check_name(&run.name)?;
            platform::run(&run.name, run.config.as_deref(), environment)
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::{Environment, Path, Registration};

    const ONLY_WINDOWS: &str =
        "services are a Windows feature; on Linux use packaging/systemd/bohio.service";

    pub(super) fn install(_: &Registration) -> Result<(), String> {
        Err(ONLY_WINDOWS.to_owned())
    }

    pub(super) fn uninstall(_: &str) -> Result<(), String> {
        Err(ONLY_WINDOWS.to_owned())
    }

    pub(super) fn run(_: &str, _: Option<&Path>, _: &Environment) -> Result<(), String> {
        Err(ONLY_WINDOWS.to_owned())
    }
}

#[cfg(windows)]
mod platform {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;

    use windows_service::service::{
        ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
        ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
    };
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
    use windows_service::{Error, service_dispatcher};

    use super::super::host;
    use super::{Environment, Path, PathBuf, Registration};
    use crate::config::Config;

    const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;
    const DESCRIPTION: &str = "Bohio: serves files, processes and shells to peers it has proven";
    /// How long the manager is told a stop may take.
    const STOP_HINT: Duration = Duration::from_secs(10);

    /// ERROR_ACCESS_DENIED: the caller is not elevated.
    const ACCESS_DENIED: i32 = 5;
    /// ERROR_SERVICE_DOES_NOT_EXIST.
    const DOES_NOT_EXIST: i32 = 1060;

    fn describe(error: &Error) -> String {
        match error {
            Error::Winapi(io) if io.raw_os_error() == Some(ACCESS_DENIED) => {
                "access denied: run this from an elevated prompt".to_owned()
            }
            Error::Winapi(io) if io.raw_os_error() == Some(DOES_NOT_EXIST) => {
                "no such service".to_owned()
            }
            other => other.to_string(),
        }
    }

    pub(super) fn install(registration: &Registration) -> Result<(), String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("cannot find this program: {error}"))?;
        let manager =
            ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)
                .map_err(|error| describe(&error))?;
        let info = ServiceInfo {
            name: registration.name.clone().into(),
            display_name: registration.name.clone().into(),
            service_type: SERVICE_TYPE,
            start_type: ServiceStartType::AutoStart,
            error_control: ServiceErrorControl::Normal,
            executable_path: executable,
            launch_arguments: registration.launch_arguments.clone(),
            dependencies: Vec::new(),
            account_name: Some(registration.account.clone().into()),
            account_password: None,
        };
        let service = manager
            .create_service(&info, ServiceAccess::CHANGE_CONFIG)
            .map_err(|error| describe(&error))?;
        service
            .set_description(DESCRIPTION)
            .map_err(|error| describe(&error))?;
        println!(
            "installed {}: `sc start {}` starts it",
            registration.name, registration.name
        );
        Ok(())
    }

    pub(super) fn uninstall(name: &str) -> Result<(), String> {
        let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
            .map_err(|error| describe(&error))?;
        let access = ServiceAccess::QUERY_STATUS | ServiceAccess::STOP | ServiceAccess::DELETE;
        let service = manager
            .open_service(name, access)
            .map_err(|error| describe(&error))?;
        if service
            .query_status()
            .map_err(|error| describe(&error))?
            .current_state
            != ServiceState::Stopped
        {
            service.stop().map_err(|error| describe(&error))?;
            for _ in 0..50 {
                let state = service
                    .query_status()
                    .map_err(|error| describe(&error))?
                    .current_state;
                if state == ServiceState::Stopped {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        service.delete().map_err(|error| describe(&error))?;
        println!("removed {name}");
        Ok(())
    }

    /// What the service's entry point, which the manager calls with no arguments of ours, needs.
    struct Launch {
        name: String,
        config: Option<PathBuf>,
        environment: Environment,
    }

    static LAUNCH: OnceLock<Launch> = OnceLock::new();
    static OUTCOME: Mutex<Option<String>> = Mutex::new(None);

    pub(super) fn run(
        name: &str,
        config: Option<&Path>,
        environment: &Environment,
    ) -> Result<(), String> {
        let _ = LAUNCH.set(Launch {
            name: name.to_owned(),
            config: config.map(Path::to_path_buf),
            environment: environment.clone(),
        });
        service_dispatcher::start(name, entry).map_err(|error| match &error {
            Error::Winapi(io) if io.raw_os_error() == Some(1063) => {
                "this command is started by the service manager: use `bohio service install`, then `sc start`".to_owned()
            }
            other => describe(other),
        })?;
        match OUTCOME.lock().ok().and_then(|mut slot| slot.take()) {
            Some(reason) => Err(reason),
            None => Ok(()),
        }
    }

    /// The entry point the service manager calls. It is safe code: the arguments the manager passes
    /// are not used, the launch arguments are in `LAUNCH`.
    extern "system" fn entry(_count: u32, _arguments: *mut *mut u16) {
        if let Some(launch) = LAUNCH.get()
            && let Err(reason) = serve(launch)
            && let Ok(mut slot) = OUTCOME.lock()
        {
            *slot = Some(reason);
        }
    }

    fn serve(launch: &Launch) -> Result<(), String> {
        let stop = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&stop);
        let handler = move |control| match control {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                signal.store(true, Ordering::SeqCst);
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        };
        let status = service_control_handler::register(&launch.name, handler)
            .map_err(|error| describe(&error))?;
        let report = |state: ServiceState, code: u32, wait: Duration| {
            let accepted = if state == ServiceState::Running {
                ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN
            } else {
                ServiceControlAccept::empty()
            };
            let _ = status.set_service_status(ServiceStatus {
                service_type: SERVICE_TYPE,
                current_state: state,
                controls_accepted: accepted,
                exit_code: ServiceExitCode::Win32(code),
                checkpoint: 0,
                wait_hint: wait,
                process_id: None,
            });
        };
        report(ServiceState::StartPending, 0, STOP_HINT);
        let started = (|| {
            let (config, from) = Config::locate(launch.config.as_deref(), &launch.environment)
                .map_err(|error| error.to_string())?;
            let entries = config.listen.clone();
            Ok::<_, String>((config, from, entries))
        })();
        let (config, from, entries) = match started {
            Ok(parts) => parts,
            Err(reason) => {
                report(ServiceState::Stopped, 1, Duration::ZERO);
                return Err(reason);
            }
        };
        // The host runs on its own thread: a connection that is mid-transfer must not hold the stop
        // back, and the process ending closes the job objects that end every process it started.
        let host_stop = Arc::clone(&stop);
        let host = std::thread::spawn({
            let environment = launch.environment.clone();
            move || {
                host::serve_endpoints(&config, from.as_deref(), &environment, &entries, &host_stop)
            }
        });
        report(ServiceState::Running, 0, Duration::ZERO);
        while !stop.load(Ordering::SeqCst) && !host.is_finished() {
            std::thread::sleep(Duration::from_millis(200));
        }
        report(ServiceState::StopPending, 0, STOP_HINT);
        stop.store(true, Ordering::SeqCst);
        let failure = if host.is_finished() {
            host.join().ok().and_then(Result::err)
        } else {
            None
        };
        report(
            ServiceState::Stopped,
            u32::from(failure.is_some()),
            Duration::ZERO,
        );
        failure.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn install(name: &str, config: Option<&str>) -> InstallArgs {
        InstallArgs {
            config: config.map(PathBuf::from),
            name: name.to_owned(),
            account: DEFAULT_ACCOUNT.to_owned(),
        }
    }

    #[test]
    fn the_service_is_started_by_running_it_with_the_name_and_an_absolute_configuration() {
        let registration = registration(&install("bohio", Some("bohio.cfg"))).unwrap();
        let arguments: Vec<_> = registration
            .launch_arguments
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(&arguments[..4], ["service", "run", "--name", "bohio"]);
        assert_eq!(arguments[4], "--config");
        assert!(Path::new(&arguments[5]).is_absolute(), "{arguments:?}");
        assert!(arguments[5].ends_with("bohio.cfg"));
        assert_eq!(registration.account, DEFAULT_ACCOUNT);
    }

    #[test]
    fn without_a_configuration_the_service_searches_the_system_location() {
        let registration = registration(&install("edge", None)).unwrap();
        assert_eq!(registration.launch_arguments.len(), 4);
        assert_eq!(registration.name, "edge");
    }

    #[test]
    fn a_name_that_is_empty_or_holds_a_separator_is_refused() {
        for bad in ["", "  ", "a/b", r"a\b"] {
            assert!(registration(&install(bad, None)).is_err(), "{bad:?}");
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn an_empty_account_is_refused() {
        let mut args = install("bohio", None);
        args.account = " ".to_owned();
        assert!(registration(&args).is_err());
    }

    #[cfg(not(windows))]
    #[test]
    fn off_windows_every_action_says_where_services_are() {
        let environment = crate::cli::capture_environment();
        for action in [
            ServiceAction::Install(install("bohio", None)),
            ServiceAction::Uninstall(NameArgs {
                name: "bohio".to_owned(),
            }),
            ServiceAction::Run(RunArgs {
                config: None,
                name: "bohio".to_owned(),
            }),
        ] {
            let error = run(&ServiceArgs { action }, &environment).unwrap_err();
            assert!(error.contains("systemd"), "{error}");
        }
    }
}
