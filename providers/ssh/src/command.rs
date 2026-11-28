// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The command line the host's shell is given, and the words of a signal.

use bohio_api::model::{EnvVar, ExecStart, ProgramSpec};

/// `word` as one word of a POSIX shell command line.
pub(crate) fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// The program and arguments that make shell `kind` run `script`: the host must have that shell.
fn script_words(kind: &str, script: &str) -> Result<Vec<String>, String> {
    match kind {
        "sh" | "bash" | "zsh" => Ok(vec![kind.to_owned(), "-c".to_owned(), script.to_owned()]),
        "powershell" | "pwsh" => Ok(vec![
            kind.to_owned(),
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            script.to_owned(),
        ]),
        other => Err(format!(
            "an ssh source runs the shell kinds sh, bash, zsh, powershell and pwsh, not `{other}`"
        )),
    }
}

/// Whether `name` can be a shell variable. A name that starts with `-` would be read by `env` as an option.
fn is_variable_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// The line that tells whether the login shell reads quotes as a POSIX shell does: `a\` is one word under POSIX
/// single quotes, while `fish` reads `\'` as an escape and `cmd.exe` and PowerShell read quotes differently.
pub(crate) const PROBE_LINE: &str = "exec printf '%s|' 'a\\' 'b c'";

/// How much of the probe's output is kept: what [`PROBE_LINE`] prints under a POSIX shell, and room before it for what an
/// rc file prints first.
pub(crate) const PROBE_KEEP: usize = 4096;

/// What the probe found out about the login shell. Only an answer that is the shell's own is kept for the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Probe {
    /// The shell read the line as POSIX does.
    Posix,
    /// The line ran to its end and printed something else: another kind of shell.
    Other,
    /// The line did not run to its end (no answer in time): this says nothing about the shell.
    Unknown,
}

/// What [`PROBE_LINE`] came to: `output` is the tail of what it printed, and `ended` says whether the program ended.
/// Anything an rc file printed before the line is ignored.
pub(crate) fn probe_verdict(output: &[u8], ended: bool) -> Probe {
    if output.ends_with(b"a\\|b c|") {
        Probe::Posix
    } else if ended {
        Probe::Other
    } else {
        Probe::Unknown
    }
}

/// Whether the line runs the program under `env`.
fn env_changes(clear_env: bool, env: &[EnvVar]) -> bool {
    clear_env || !env.is_empty()
}

/// What goes before the program: `cd DIR && exec env ...`. `env` is on the line because most servers refuse the `env`
/// channel request.
fn prefix(cwd: Option<&str>, clear_env: bool, env: &[EnvVar]) -> Result<String, String> {
    if cwd.is_some_and(|cwd| cwd.contains('\0')) {
        return Err("the working directory holds a NUL".to_owned());
    }
    let mut text = String::new();
    if let Some(cwd) = cwd {
        // `--` keeps a directory named `-` or `-x` from being read as an option.
        text.push_str("cd -- ");
        text.push_str(&quote(cwd));
        text.push_str(" && ");
    }
    text.push_str("exec ");
    if env_changes(clear_env, env) {
        text.push_str("env");
        if clear_env {
            text.push_str(" -i");
        }
        // `env` takes its options before its assignments.
        for change in env {
            if !is_variable_name(&change.name) {
                return Err(format!("`{}` is not a variable name", change.name));
            }
            if change.value.is_none() {
                text.push_str(" -u ");
                text.push_str(&quote(&change.name));
            }
        }
        // `--` ends the options, so that nothing after it is read as one.
        text.push_str(" --");
        for change in env {
            if let Some(value) = &change.value {
                if value.contains('\0') {
                    return Err("a variable's value holds a NUL".to_owned());
                }
                text.push(' ');
                text.push_str(&quote(&format!("{}={value}", change.name)));
            }
        }
        text.push(' ');
    }
    Ok(text)
}

/// The line the host's shell runs: `cd DIR && exec env ... PROGRAM ARGS`. Every word is quoted, so the host's login
/// shell must be a POSIX one.
pub(crate) fn line(
    words: &[String],
    cwd: Option<&str>,
    clear_env: bool,
    env: &[EnvVar],
) -> Result<String, String> {
    if words.is_empty() {
        return Err("the program is empty".to_owned());
    }
    if words.iter().any(|word| word.contains('\0')) {
        return Err("a word of the command holds a NUL".to_owned());
    }
    let mut text = prefix(cwd, clear_env, env)?;
    let mut words = words.to_vec();
    // A program word that starts with `-` would be read as an option by `exec` or `env`: it names a file of the
    // working directory.
    if words[0].starts_with('-') {
        words[0].insert_str(0, "./");
    }
    if env_changes(clear_env, env) && words[0].contains('=') {
        // `env` reads every operand with an `=` as an assignment, so a shell starts the program instead.
        words.splice(0..0, ["sh", "-c", "exec \"$@\"", "sh"].map(str::to_owned));
    }
    let rendered: Vec<String> = words.iter().map(|word| quote(word)).collect();
    text.push_str(&rendered.join(" "));
    Ok(text)
}

/// The line that starts the account's own login shell where `cwd` and `env` say.
pub(crate) fn login_shell_line(
    cwd: Option<&str>,
    clear_env: bool,
    env: &[EnvVar],
) -> Result<String, String> {
    let mut text = prefix(cwd, clear_env, env)?;
    text.push_str("\"${SHELL:-sh}\" -l");
    Ok(text)
}

/// The words of a program an exec request names: its argument vector, or the shell that runs its script.
pub(crate) fn exec_words(request: &ExecStart) -> Result<Vec<String>, String> {
    match (&request.shell, &request.script) {
        (Some(kind), Some(script)) => script_words(kind, script),
        (None, None) => Ok(request.argv.clone()),
        _ => Err("a script needs a shell kind and a shell kind needs a script".to_owned()),
    }
}

/// The words of the program a terminal request names; `None` for the host's own login shell.
pub(crate) fn terminal_words(program: &ProgramSpec) -> Result<Option<Vec<String>>, String> {
    if let Some(kind) = &program.kind {
        return match kind.as_str() {
            "sh" | "bash" | "zsh" | "pwsh" | "powershell" => Ok(Some(vec![kind.clone()])),
            other => Err(format!(
                "an ssh source starts the shells sh, bash, zsh, pwsh and powershell, not `{other}`"
            )),
        };
    }
    if program.argv.is_empty() {
        return Err("the program is empty".to_owned());
    }
    Ok(Some(program.argv.clone()))
}

/// The signal number of an SSH signal name; `None` for one without a stable number.
pub(crate) fn signal_number(name: &str) -> Option<i64> {
    Some(match name {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "ILL" => 4,
        "ABRT" => 6,
        "FPE" => 8,
        "KILL" => 9,
        "USR1" => 10,
        "SEGV" => 11,
        "USR2" => 12,
        "PIPE" => 13,
        "ALRM" => 14,
        "TERM" => 15,
        _ => return None,
    })
}

/// `(os, arch)` from the output of `uname -sm`, named as `std::env::consts` names them.
pub(crate) fn parse_uname(output: &str) -> (Option<String>, Option<String>) {
    let mut words = output.split_whitespace();
    let os = words
        .next()
        .map(|name| match name.to_ascii_lowercase().as_str() {
            "darwin" => "macos".to_owned(),
            other => other.to_owned(),
        });
    let arch = words.next().map(|name| match name {
        "arm64" | "aarch64" => "aarch64".to_owned(),
        "amd64" | "x86_64" => "x86_64".to_owned(),
        "i386" | "i686" | "i586" => "x86".to_owned(),
        other => other.to_owned(),
    });
    (os, arch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uname_output_is_named_as_rust_names_it() {
        let some = |text: &str| Some(text.to_owned());
        assert_eq!(
            parse_uname(
                "Linux aarch64
"
            ),
            (some("linux"), some("aarch64"))
        );
        assert_eq!(
            parse_uname(
                "Darwin arm64
"
            ),
            (some("macos"), some("aarch64"))
        );
        assert_eq!(
            parse_uname("FreeBSD amd64"),
            (some("freebsd"), some("x86_64"))
        );
        assert_eq!(parse_uname(""), (None, None));
    }

    #[test]
    fn every_word_is_one_word_of_the_shell() {
        let words = vec!["echo".to_owned(), "it's a $HOME".to_owned()];
        assert_eq!(
            line(&words, None, false, &[]).unwrap(),
            "exec 'echo' 'it'\\''s a $HOME'"
        );
    }

    #[test]
    fn the_directory_and_the_environment_are_on_the_line() {
        let env = [
            EnvVar {
                name: "A".to_owned(),
                value: Some("1".to_owned()),
            },
            EnvVar {
                name: "B".to_owned(),
                value: None,
            },
        ];
        assert_eq!(
            line(&["pwd".to_owned()], Some("/tmp/x y"), true, &env).unwrap(),
            "cd -- '/tmp/x y' && exec env -i -u 'B' -- 'A=1' 'pwd'"
        );
    }

    #[test]
    fn the_probe_ignores_what_an_rc_file_printed_and_keeps_only_a_conclusive_answer() {
        assert_eq!(probe_verdict(b"a\\|b c|", true), Probe::Posix);
        assert_eq!(
            probe_verdict(b"Last login: today\r\na\\|b c|", true),
            Probe::Posix
        );
        assert_eq!(probe_verdict(b"a\\|b c|", false), Probe::Posix);
        assert_eq!(probe_verdict(b"", true), Probe::Other);
        assert_eq!(probe_verdict(b"a' 'b c|", true), Probe::Other);
        // No output in time says nothing about the shell.
        assert_eq!(probe_verdict(b"", false), Probe::Unknown);
    }

    #[test]
    fn a_program_word_is_never_read_as_an_option_or_an_assignment() {
        let one = [EnvVar {
            name: "A".to_owned(),
            value: Some("1".to_owned()),
        }];
        // A directory or a program that starts with `-` is a name, not an option of `cd`, `exec` or `env`.
        assert_eq!(
            line(&["-l".to_owned()], Some("-"), false, &[]).unwrap(),
            "cd -- '-' && exec './-l'"
        );
        assert_eq!(
            line(&["-l".to_owned()], None, false, &one).unwrap(),
            "exec env -- 'A=1' './-l'"
        );
        // A program word with an `=` is not an assignment of `env`: a shell starts it.
        assert_eq!(
            line(&["X=y/echo".to_owned(), "hi".to_owned()], None, false, &one).unwrap(),
            "exec env -- 'A=1' 'sh' '-c' 'exec \"$@\"' 'sh' 'X=y/echo' 'hi'"
        );
        // Without `env` the word is quoted and `exec` takes it as it is.
        assert_eq!(
            line(&["X=y".to_owned()], None, false, &[]).unwrap(),
            "exec 'X=y'"
        );
    }

    #[test]
    fn a_word_with_a_nul_or_a_bad_variable_is_refused() {
        assert!(line(&["a\0".to_owned()], None, false, &[]).is_err());
        let bad = [EnvVar {
            name: "A=B".to_owned(),
            value: Some("1".to_owned()),
        }];
        assert!(line(&["a".to_owned()], None, false, &bad).is_err());
        assert!(line(&[], None, false, &[]).is_err());
    }

    #[test]
    fn a_variable_name_that_env_would_read_as_an_option_is_refused() {
        for name in ["-S", "A B", "1A", ""] {
            let option = [EnvVar {
                name: name.to_owned(),
                value: Some("x".to_owned()),
            }];
            assert!(
                line(&["a".to_owned()], None, false, &option).is_err(),
                "{name:?}"
            );
        }
        let fine = [EnvVar {
            name: "_A1".to_owned(),
            value: Some("x".to_owned()),
        }];
        assert!(line(&["a".to_owned()], None, false, &fine).is_ok());
    }
}
