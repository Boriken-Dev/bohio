//! Turning a request into the program and arguments to start.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use bohio_proto::exec::{Command, Request};
use bohio_proto::shell::ShellKind;
use bohio_proto::{ErrorBody, ErrorCode};

use crate::platform::{Spec, resolve_listed, touches_search};
use crate::policy::Grant;

/// The process to start for `request`, once `grant` allows it.
///
/// # Errors
///
/// `bad_request` for an empty argument vector or an unknown shell, `denied` for a program the
/// grant does not list, for a request that changes `PATH` or clears the environment under a
/// program list, or for a shell script when the grant lists any. Under a list the program that
/// starts is the one the host finds at the listed name on its own `path`, never one the request
/// can point elsewhere.
pub(super) fn spec(request: &Request, grant: &Grant, path: &OsStr) -> Result<Spec, ErrorBody> {
    let (program, args, raw_args) = match &request.command {
        Command::Argv { argv } => {
            let Some((program, args)) = argv.split_first() else {
                return Err(ErrorBody::new(
                    ErrorCode::BadRequest,
                    "the argument vector is empty",
                ));
            };
            if program.is_empty() || program.contains('\0') || args.iter().any(|a| a.contains('\0'))
            {
                return Err(ErrorBody::new(
                    ErrorCode::BadRequest,
                    "the program or an argument is empty or holds a NUL",
                ));
            }
            if !grant.permits_program(program) {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    "this peer may not run that program",
                ));
            }
            let program = if grant.restricts_programs() {
                if touches_search(&request.env) {
                    return Err(ErrorBody::new(
                        ErrorCode::Denied,
                        "this peer may not change PATH or clear the environment",
                    ));
                }
                resolve_listed(program, path)
                    .ok_or_else(|| {
                        ErrorBody::new(ErrorCode::NotFound, "the program was not found")
                    })?
                    .into_os_string()
            } else {
                OsString::from(program)
            };
            (program, args.iter().map(OsString::from).collect(), None)
        }
        Command::Shell { kind, script } => {
            if !grant.permits_shell_script() {
                return Err(ErrorBody::new(
                    ErrorCode::Denied,
                    "this peer may run only listed programs, not shell scripts",
                ));
            }
            if script.contains('\0') {
                return Err(ErrorBody::new(
                    ErrorCode::BadRequest,
                    "the script holds a NUL",
                ));
            }
            shell_line(*kind, script)?
        }
        _ => {
            return Err(ErrorBody::new(
                ErrorCode::Unsupported,
                "this host does not know that kind of command",
            ));
        }
    };
    Ok(Spec {
        program,
        args,
        raw_args,
        cwd: request.cwd.as_ref().map(PathBuf::from),
        env: request.env.clone(),
        stdin: matches!(request.stdin, bohio_proto::exec::Stdin::Open),
    })
}

type Line = (OsString, Vec<OsString>, Option<OsString>);

/// The program and arguments that make `kind` run `script`.
fn shell_line(kind: ShellKind, script: &str) -> Result<Line, ErrorBody> {
    let program = OsString::from(kind.program());
    Ok(match kind {
        ShellKind::Sh | ShellKind::Bash | ShellKind::Zsh => {
            (program, vec!["-c".into(), script.into()], None)
        }
        ShellKind::Powershell | ShellKind::Pwsh => (
            program,
            vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                script.into(),
            ],
            None,
        ),
        // `cmd` parses its own command line: the script goes through as written.
        ShellKind::Cmd if cfg!(windows) => (
            program,
            vec!["/C".into()],
            Some(OsString::from(format!(" {script}"))),
        ),
        ShellKind::Cmd => (program, vec!["/C".into(), script.into()], None),
        _ => {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "that shell kind is not known to this host",
            ));
        }
    })
}
