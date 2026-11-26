//! Putting the node's own helper program on a machine an exec source reaches, so a machine with no Bohío installed can
//! still serve its desktop.
//!
//! The file goes to a directory the operator names, over the source's files, under a name that holds its digest; it is
//! made only when the machine's platform (`uname -sm`) is the one the file was built for.

#![forbid(unsafe_code)]

use std::fs::File as LocalFile;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use bohio_api::files::{failure_kind, word};
use bohio_api::guatiao::library::Object;
use bohio_api::model::{ExecStart, OpenModel};
use bohio_api::{Directory, File as RemoteFileKind, Process};
use bohio_proto::hash::Algorithm;
use bohio_transfer::hash::digest_reader;

use crate::access::Sources;

/// How long one read of a process waits before the stop flag is looked at again.
const POLL_MS: u64 = 200;
/// Bytes of the file one write carries.
const CHUNK: usize = 1024 * 1024;
/// Bytes of the file's start that name its platform.
const HEAD: usize = 4096;

/// Where and how a helper is put on a machine (`[gui] helper_upload` and the settings beside it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Upload {
    /// The files source that holds the directory; absent, the exec source's own name.
    pub files: Option<String>,
    /// The directory the file goes to, in the files source's own syntax: it exists.
    pub dir: String,
    /// Ask the machine what it runs (`uname -sm` through the exec source) and refuse a file built for another platform.
    pub check_platform: bool,
    /// The file to put there; absent, the node's own executable.
    pub binary: Option<PathBuf>,
    /// Replace a file of the same name that is not this one.
    pub overwrite: bool,
    /// Accept a directory that another account can write to.
    pub shared_dir: bool,
    /// The most bytes of the machine's answer to `uname -sm` that are read; more fails the upload.
    pub answer_max: usize,
}

/// The kind of executable a file is, and the processors it holds code for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    /// `elf`, `mach-o` or `pe`.
    pub format: &'static str,
    /// Names as `uname -m` says them on a Linux machine (`x86_64`, `aarch64`, `arm`, `x86`, `riscv64`, `ppc64`, `s390x`).
    pub arches: Vec<&'static str>,
}

fn arch_of_machine(machine: u16) -> Option<&'static str> {
    Some(match machine {
        0x03 => "x86",
        0x28 => "arm",
        0x3E => "x86_64",
        0xB7 => "aarch64",
        0xF3 => "riscv64",
        0x15 | 0x14 => "ppc64",
        0x16 => "s390x",
        _ => return None,
    })
}

fn arch_of_cpu(cpu: u32) -> Option<&'static str> {
    Some(match cpu {
        0x0100_0007 => "x86_64",
        0x0100_000C => "aarch64",
        7 => "x86",
        12 => "arm",
        _ => return None,
    })
}

/// What `head`, the first bytes of a file, says the file is built for.
///
/// # Errors
///
/// The file is not an ELF, Mach-O or PE executable of a processor this knows.
pub fn built_for(head: &[u8]) -> Result<Built, String> {
    let u16_at = |at: usize, big: bool| -> Option<u16> {
        let bytes: [u8; 2] = head.get(at..at + 2)?.try_into().ok()?;
        Some(if big {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        })
    };
    let u32_at = |at: usize, big: bool| -> Option<u32> {
        let bytes: [u8; 4] = head.get(at..at + 4)?.try_into().ok()?;
        Some(if big {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        })
    };
    let unknown =
        || "the file is not an executable of a kind this knows (ELF, Mach-O or PE)".to_owned();
    let (format, arch) = if head.starts_with(b"\x7fELF") {
        let big = head.get(5) == Some(&2);
        ("elf", u16_at(18, big).and_then(arch_of_machine))
    } else if head.starts_with(b"MZ") {
        let at =
            usize::try_from(u32_at(0x3C, false).ok_or_else(unknown)?).map_err(|_| unknown())?;
        if head.get(at..at + 4) != Some(b"PE\0\0") {
            return Err(unknown());
        }
        let arch = match u16_at(at + 4, false) {
            Some(0x8664) => Some("x86_64"),
            Some(0xAA64) => Some("aarch64"),
            Some(0x14C) => Some("x86"),
            _ => None,
        };
        ("pe", arch)
    } else if head.starts_with(&[0xCF, 0xFA, 0xED, 0xFE])
        || head.starts_with(&[0xCE, 0xFA, 0xED, 0xFE])
    {
        ("mach-o", u32_at(4, false).and_then(arch_of_cpu))
    } else if head.starts_with(&[0xCA, 0xFE, 0xBA, 0xBE]) {
        let count = usize::try_from(u32_at(4, true).ok_or_else(unknown)?).map_err(|_| unknown())?;
        let arches: Vec<_> = (0..count.min(16))
            .filter_map(|n| u32_at(8 + 20 * n, true).and_then(arch_of_cpu))
            .collect();
        if arches.is_empty() {
            return Err(unknown());
        }
        return Ok(Built {
            format: "mach-o",
            arches,
        });
    } else {
        return Err(unknown());
    };
    let arch =
        arch.ok_or_else(|| "the file is built for a processor this does not know".to_owned())?;
    Ok(Built {
        format,
        arches: vec![arch],
    })
}

/// What `uname -sm` answered, as the kind of executable the machine runs and its processor's name as `built_for` says it.
///
/// # Errors
///
/// The answer is not two words, or names a system or processor this does not know.
pub fn machine_runs(answer: &str) -> Result<(&'static str, &'static str), String> {
    let mut words = answer.split_whitespace();
    let (Some(system), Some(machine), None) = (words.next(), words.next(), words.next()) else {
        return Err(format!("`uname -sm` answered `{}`", answer.trim()));
    };
    let upper = system.to_ascii_uppercase();
    let format = if upper.starts_with("MINGW")
        || upper.starts_with("MSYS")
        || upper.starts_with("CYGWIN")
        || upper.starts_with("WINDOWS")
    {
        "pe"
    } else if upper == "DARWIN" {
        "mach-o"
    } else if [
        "LINUX",
        "FREEBSD",
        "NETBSD",
        "OPENBSD",
        "DRAGONFLY",
        "SUNOS",
        "HAIKU",
    ]
    .contains(&upper.as_str())
    {
        "elf"
    } else {
        return Err(format!("the system `{system}` is not one this knows"));
    };
    let lower = machine.to_ascii_lowercase();
    let arch = match lower.as_str() {
        "x86_64" | "amd64" | "x64" => "x86_64",
        "aarch64" | "arm64" | "aarch64_be" => "aarch64",
        "i386" | "i486" | "i586" | "i686" | "x86" => "x86",
        "riscv64" => "riscv64",
        "ppc64" | "ppc64le" | "powerpc64" | "powerpc64le" => "ppc64",
        "s390x" => "s390x",
        other if other.starts_with("armv") || other == "arm" => "arm",
        other => return Err(format!("the processor `{other}` is not one this knows")),
    };
    Ok((format, arch))
}

/// Refuses a file built for something other than what the machine runs.
///
/// # Errors
///
/// What the file is built for and what the machine runs.
pub fn check_platform(built: &Built, answer: &str) -> Result<(), String> {
    let (format, arch) = machine_runs(answer)?;
    if built.format == format && built.arches.contains(&arch) {
        return Ok(());
    }
    Err(format!(
        "the helper is a {} executable for {} and the machine runs {format} for {arch} (`uname -sm`: {})",
        built.format,
        built.arches.join(" or "),
        answer.trim()
    ))
}

/// Runs `argv` on the exec source and answers its standard output when it exits with status 0.
fn capture(
    sources: &Sources,
    source: &str,
    argv: &[&str],
    stop: &AtomicBool,
    max: usize,
) -> Result<String, String> {
    let exec = sources.exec(Some(source)).map_err(|error| error.message)?;
    let process: Object<dyn Process> = exec
        .get()
        .start(ExecStart {
            argv: argv.iter().map(|word| (*word).to_owned()).collect(),
            ..ExecStart::default()
        })
        .map_err(|error| error.message().to_string())?;
    let mut out = Vec::new();
    let mut buffer = [0_u8; 1024];
    loop {
        if stop.load(Ordering::SeqCst) {
            let _ = process.kill();
            return Err("stopped".to_owned());
        }
        match process.read(1, &mut buffer, POLL_MS) {
            Ok(0) => {}
            Ok(count) if count > 0 => {
                out.extend_from_slice(&buffer[..usize::try_from(count).unwrap_or(0)]);
                if out.len() > max {
                    let _ = process.kill();
                    return Err(format!(
                        "`{}` answered with more than the {max} bytes `[gui] helper_upload_answer_max` allows",
                        argv.join(" ")
                    ));
                }
            }
            Ok(_) => break,
            Err(error) => return Err(error.message().to_string()),
        }
    }
    let exit = loop {
        if stop.load(Ordering::SeqCst) {
            let _ = process.kill();
            return Err("stopped".to_owned());
        }
        match process.wait(POLL_MS) {
            Ok(Some(exit)) => break exit,
            Ok(None) => {}
            Err(error) => return Err(error.message().to_string()),
        }
    };
    if exit.code != Some(0) {
        return Err(format!("`{}` ended with {exit:?}", argv.join(" ")));
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// A file of the machine read from its start.
struct Remote<'a> {
    file: &'a Object<dyn RemoteFileKind>,
    at: u64,
}

impl Read for Remote<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self
            .file
            .read_at(self.at, buffer)
            .map_err(|error| io::Error::other(error.message().to_string()))?;
        self.at += count;
        Ok(usize::try_from(count).unwrap_or(0))
    }
}

fn sha256(reader: &mut impl Read) -> io::Result<String> {
    let (digests, _) = digest_reader(reader, &[Algorithm::Sha256], 64 * 1024)?;
    digests
        .get(&Algorithm::Sha256)
        .cloned()
        .ok_or_else(|| io::Error::other("no digest"))
}

fn text(error: &bohio_api::guatiao::library::ProviderError) -> String {
    error.message().to_string()
}

/// A system with no permission bits has nothing to set: that is not a failure.
fn executable(done: Result<(), bohio_api::guatiao::library::ProviderError>) -> Result<(), String> {
    match done {
        Err(error) if failure_kind(&error) != Some(word::UNSUPPORTED) => Err(text(&error)),
        _ => Ok(()),
    }
}

/// Puts the helper on the machine `source` reaches and answers its path there.
///
/// An existing file of the name is used when it holds this very file, and refused unless `overwrite` when it does not.
///
/// # Errors
///
/// The platform does not match, the directory is not one the file may go to, the file is there and is not this one, or the
/// machine failed.
pub fn upload(
    sources: &Sources,
    source: &str,
    upload: &Upload,
    stop: &AtomicBool,
) -> Result<String, String> {
    let binary = match &upload.binary {
        Some(path) => path.clone(),
        None => {
            std::env::current_exe().map_err(|error| format!("this node's own file: {error}"))?
        }
    };
    let shown = |error: io::Error| format!("{}: {error}", Path::new(&binary).display());
    let mut local = LocalFile::open(&binary).map_err(shown)?;
    let mut head = vec![0_u8; HEAD];
    let mut got = 0;
    while got < HEAD {
        let count = local.read(&mut head[got..]).map_err(shown)?;
        if count == 0 {
            break;
        }
        got += count;
    }
    head.truncate(got);
    let built = built_for(&head).map_err(|why| format!("{}: {why}", binary.display()))?;
    if upload.check_platform {
        let answer = capture(sources, source, &["uname", "-sm"], stop, upload.answer_max)?;
        check_platform(&built, &answer)?;
    }

    let mut local = LocalFile::open(&binary).map_err(shown)?;
    let digest = sha256(&mut local).map_err(shown)?;
    let size = local.metadata().map_err(shown)?.len();
    // A Windows machine runs a file by its extension.
    let extension = binary
        .extension()
        .and_then(|extension| extension.to_str())
        .filter(|extension| extension.eq_ignore_ascii_case("exe"))
        .map_or_else(String::new, |extension| format!(".{extension}"));
    let name = format!("bohio-helper-{}{extension}", &digest[..16]);
    let part = format!(".{name}.part");

    let files = sources
        .files(Some(upload.files.as_deref().unwrap_or(source)))
        .map_err(|error| error.message)?;
    let dir: Object<dyn Directory> = files
        .get()
        .open_dir(&upload.dir)
        .map_err(|error| format!("{}: {}", upload.dir, text(&error)))?;
    let stat = dir.stat(".", true).map_err(|error| text(&error))?;
    if !upload.shared_dir {
        match stat.mode {
            Some(mode) if mode & 0o022 != 0 => {
                return Err(format!(
                    "{} is writable by other accounts (mode {:o}): one of them could replace the helper before it runs; \
                     `[gui] helper_upload_shared_dir = true` accepts that",
                    upload.dir,
                    mode & 0o7777
                ));
            }
            Some(_) => {}
            None => {
                return Err(format!(
                    "the source does not say who may write {}: another account may be able to replace the helper before it \
                     runs; `[gui] helper_upload_shared_dir = true` accepts that",
                    upload.dir
                ));
            }
        }
    }
    for (weakened, key) in [
        (upload.overwrite, "helper_upload_overwrite"),
        (upload.shared_dir, "helper_upload_shared_dir"),
        (!upload.check_platform, "helper_upload_check_platform"),
    ] {
        if weakened {
            tracing::warn!(
                source,
                "non-default: [gui] {key} weakens a check of the helper's upload"
            );
        }
    }

    let mut replacing = false;
    match dir.stat(&name, false) {
        Ok(found) => {
            let same = found.kind == "file" && found.size == size && {
                let file = dir
                    .open_file(
                        &name,
                        OpenModel {
                            read: true,
                            ..OpenModel::default()
                        },
                    )
                    .map_err(|error| text(&error))?;
                sha256(&mut Remote { file: &file, at: 0 }).is_ok_and(|theirs| theirs == digest)
            };
            if same {
                executable(dir.set_mode(&name, 0o700))?;
                return Ok(format!("{}/{name}", upload.dir.trim_end_matches('/')));
            }
            if !upload.overwrite {
                return Err(format!(
                    "{}/{name} exists and is not this helper; `[gui] helper_upload_overwrite = true` replaces it",
                    upload.dir
                ));
            }
            replacing = true;
        }
        Err(error) if failure_kind(&error) == Some(word::NOT_FOUND) => {}
        Err(error) => return Err(text(&error)),
    }

    let target = dir
        .open_file(
            &part,
            OpenModel {
                write: true,
                create: true,
                truncate: true,
                mode: Some(0o700),
                ..OpenModel::default()
            },
        )
        .map_err(|error| text(&error))?;
    let mut source_file = LocalFile::open(&binary).map_err(shown)?;
    let mut buffer = vec![0_u8; CHUNK];
    let mut at = 0_u64;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err("stopped".to_owned());
        }
        let count = source_file.read(&mut buffer).map_err(shown)?;
        if count == 0 {
            break;
        }
        let mut done = 0;
        while done < count {
            let taken = target
                .write_at(at, &buffer[done..count])
                .map_err(|error| text(&error))?;
            if taken == 0 {
                return Err("the machine took no more of the file".to_owned());
            }
            done += usize::try_from(taken).unwrap_or(count - done);
            at += taken;
        }
    }
    executable(target.set_mode(0o700))?;
    target.sync().or_else(|error| {
        if failure_kind(&error) == Some(word::UNSUPPORTED) {
            Ok(())
        } else {
            Err(text(&error))
        }
    })?;
    drop(target);
    dir.rename(&part, &name, replacing)
        .map_err(|error| text(&error))?;
    Ok(format!("{}/{name}", upload.dir.trim_end_matches('/')))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A program that writes without end, as a hostile machine's `uname` would.
    fn endless() -> Vec<String> {
        if cfg!(windows) {
            ["cmd.exe", "/c", "dir", "/s", r"C:\Windows\System32"]
                .map(str::to_owned)
                .to_vec()
        } else {
            vec!["yes".to_owned()]
        }
    }

    #[test]
    fn a_machine_that_answers_without_end_fails_the_read_at_the_bound() {
        let sources = Sources::from_config(
            &crate::config::SourceTables::new(),
            &crate::config::AccessConfig {
                desktop: Some(Vec::new()),
                ..crate::config::AccessConfig::default()
            },
        )
        .expect("the node's own sources");
        let stop = AtomicBool::new(false);
        let argv = endless();
        let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
        let failed = capture(&sources, "native", &argv, &stop, 4096).unwrap_err();
        assert!(failed.contains("helper_upload_answer_max"), "{failed}");
    }

    fn elf(machine: u16) -> Vec<u8> {
        let mut head = vec![0_u8; 64];
        head[..4].copy_from_slice(b"\x7fELF");
        head[5] = 1;
        head[18..20].copy_from_slice(&machine.to_le_bytes());
        head
    }

    fn pe(machine: u16) -> Vec<u8> {
        let mut head = vec![0_u8; 256];
        head[..2].copy_from_slice(b"MZ");
        head[0x3C..0x40].copy_from_slice(&0x80_u32.to_le_bytes());
        head[0x80..0x84].copy_from_slice(b"PE\0\0");
        head[0x84..0x86].copy_from_slice(&machine.to_le_bytes());
        head
    }

    #[test]
    fn a_header_names_what_the_file_is_built_for() {
        let built = built_for(&elf(0xB7)).expect("elf");
        assert_eq!((built.format, built.arches), ("elf", vec!["aarch64"]));
        let built = built_for(&pe(0x8664)).expect("pe");
        assert_eq!((built.format, built.arches), ("pe", vec!["x86_64"]));
        let mut macho = vec![0xCF, 0xFA, 0xED, 0xFE];
        macho.extend_from_slice(&0x0100_000C_u32.to_le_bytes());
        let built = built_for(&macho).expect("mach-o");
        assert_eq!((built.format, built.arches), ("mach-o", vec!["aarch64"]));
        let mut fat = vec![0xCA, 0xFE, 0xBA, 0xBE, 0, 0, 0, 2];
        for cpu in [0x0100_0007_u32, 0x0100_000C] {
            fat.extend_from_slice(&cpu.to_be_bytes());
            fat.extend_from_slice(&[0; 16]);
        }
        assert_eq!(built_for(&fat).expect("fat").arches, ["x86_64", "aarch64"]);
        assert!(built_for(b"#!/bin/sh\n").is_err());
        assert!(built_for(&elf(0xFFFF)).is_err());
    }

    #[test]
    fn a_machine_that_runs_another_platform_is_refused_and_the_same_is_not() {
        let linux_arm = built_for(&elf(0xB7)).unwrap();
        check_platform(&linux_arm, "Linux aarch64\n").expect("same");
        let wrong_arch = check_platform(&linux_arm, "Linux x86_64\n").unwrap_err();
        assert!(
            wrong_arch.contains("x86_64") && wrong_arch.contains("aarch64"),
            "{wrong_arch}"
        );
        assert!(check_platform(&linux_arm, "Darwin arm64\n").is_err());
        let windows = built_for(&pe(0x8664)).unwrap();
        assert!(check_platform(&windows, "Linux x86_64\n").is_err());
        check_platform(&windows, "MINGW64_NT-10.0-26100 x86_64\n").expect("a Windows shell");
        let bsd = built_for(&elf(0x3E)).unwrap();
        check_platform(&bsd, "FreeBSD amd64\n").expect("amd64 is x86_64");
        assert!(check_platform(&linux_arm, "garbage").is_err());
        assert!(check_platform(&linux_arm, "'uname' is not recognized").is_err());
    }
}
