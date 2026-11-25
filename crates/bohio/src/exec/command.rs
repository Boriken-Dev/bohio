//! Turning a request into the program and arguments to start.

use std::ffi::OsStr;

use bohio_api::model::{EnvVar, ExecStart};
use bohio_proto::exec::{Command, Request};
use bohio_proto::shell::ShellKind;
use bohio_proto::{ErrorBody, ErrorCode};

use crate::policy::Grant;

/// The process to start for `request`, once `grant` allows it.
///
/// # Errors
///
/// `bad_request` for an empty argument vector or an unknown shell, `denied` for a program the
/// grant does not list, for a request that changes a variable the grant does not name (`PATH`
/// too, unless it is named), or clears the environment under a program list, or for a shell script when the grant lists
/// any. Under a list the program that
/// starts is the one the host finds at the listed name on its own `path`, never one the request
/// can point elsewhere; finding it is the source's.
pub(super) fn spec(request: &Request, grant: &Grant, path: &OsStr) -> Result<ExecStart, ErrorBody> {
    let mut start = ExecStart {
        cwd: request.cwd.clone(),
        clear_env: request.env.clear,
        env: changes(request),
        stdin: matches!(request.stdin, bohio_proto::exec::Stdin::Open),
        path: Some(path.to_string_lossy().into_owned()),
        ..ExecStart::default()
    };
    match &request.command {
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
            if grant.restricts_programs() {
                if !grant.permits_env(&request.env) {
                    return Err(env_refused());
                }
                start.listed = true;
            }
            start.argv = argv.clone();
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
            start.shell = Some(shell_word(*kind)?.to_owned());
            start.script = Some(script.clone());
        }
        _ => {
            return Err(ErrorBody::new(
                ErrorCode::Unsupported,
                "this host does not know that kind of command",
            ));
        }
    }
    Ok(start)
}

/// The request's environment changes in the order a source applies them: sets, then removals.
fn changes(request: &Request) -> Vec<EnvVar> {
    let mut changes: Vec<EnvVar> = request
        .env
        .set
        .iter()
        .map(|(name, value)| EnvVar {
            name: name.clone(),
            value: Some(value.clone()),
        })
        .collect();
    changes.extend(request.env.remove.iter().map(|name| EnvVar {
        name: name.clone(),
        value: None,
    }));
    changes
}

/// Whether `grant` allows the command of `request` by its program list: what a running process is held
/// to when its grant changes. The `exec` permission itself is the admission's to check.
pub(super) fn permitted(request: &Request, grant: &Grant) -> bool {
    match &request.command {
        Command::Argv { argv } => {
            argv.first()
                .is_none_or(|program| grant.permits_program(program))
                && grant.permits_env(&request.env)
        }
        Command::Shell { .. } => grant.permits_shell_script(),
        _ => true,
    }
}

/// The refusal of an environment change a program list does not allow.
fn env_refused() -> ErrorBody {
    ErrorBody::new(
        ErrorCode::Denied,
        "under a program list a peer may change only the variables its grant names, and may not clear the environment",
    )
}

/// The word a source knows the shell `kind` by.
fn shell_word(kind: ShellKind) -> Result<&'static str, ErrorBody> {
    Ok(match kind {
        ShellKind::Sh => "sh",
        ShellKind::Bash => "bash",
        ShellKind::Zsh => "zsh",
        ShellKind::Powershell => "powershell",
        ShellKind::Pwsh => "pwsh",
        ShellKind::Cmd => "cmd",
        _ => {
            return Err(ErrorBody::new(
                ErrorCode::BadRequest,
                "that shell kind is not known to this host",
            ));
        }
    })
}
