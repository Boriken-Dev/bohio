//! A helper that runs on another machine, reached through a process of an exec source (an ssh source's login), and speaks
//! `bohio-gui-helper/1` over that process's standard input and output.
//!
//! The helper is `bohio gui-helper --stdio` on the machine. The account check of the pipe helper does not apply: what
//! authenticates the helper is the source's own login, and what the node trusts is the account that source logs in as.

#![forbid(unsafe_code)]

use std::io::{self, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use bohio_api::Process;
use bohio_api::guatiao::library::Object;
use bohio_api::model::ExecStart;
use coqui_transport::{Channel as _, Duplex};

use super::helper_upload::{self, Upload};
use crate::node::Node;

/// How long one read of the process waits before it looks at the stop flag again.
const POLL_MS: u64 = 200;

/// A helper to run: the exec source that reaches the machine, and the command that starts the helper there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecHelper {
    /// The exec source's name.
    pub source: String,
    /// The helper's program and arguments, run directly (never through a shell of the node).
    pub command: Vec<String>,
    /// Put the node's own helper on the machine first: the command's `{helper}` is where it went.
    pub upload: Option<Upload>,
}

/// What a command word names where the uploaded helper is.
pub const HELPER_WORD: &str = "{helper}";

impl ExecHelper {
    /// The helpers `[gui]` names: each `source` entry, with the upload `helper_upload` gives its source.
    ///
    /// # Errors
    ///
    /// The key and what is wrong with it.
    pub fn list(gui: &crate::config::GuiConfig) -> Result<Vec<Self>, (&'static str, String)> {
        let mut helpers = Vec::new();
        for text in &gui.source {
            helpers.push(Self::parse(text).map_err(|why| {
                (
                    "gui.source",
                    format!("must be NAME, an exec source, or NAME=COMMAND with the command line that starts the helper: {why}"),
                )
            })?);
        }
        let mut uploads = Vec::new();
        for text in &gui.helper_upload {
            let bad = |why: &str| {
                (
                    "gui.helper_upload",
                    format!("`{text}` must be SOURCE=DIRECTORY or SOURCE@FILES=DIRECTORY: {why}"),
                )
            };
            let (names, dir) = text.split_once('=').ok_or_else(|| bad("no ="))?;
            let (source, files) = match names.split_once('@') {
                Some((source, files)) => (source.trim(), Some(files.trim().to_owned())),
                None => (names.trim(), None),
            };
            if source.is_empty() || files.as_deref() == Some("") || dir.trim().is_empty() {
                return Err(bad("a name or the directory is empty"));
            }
            uploads.push((
                source.to_owned(),
                Upload {
                    files,
                    dir: dir.trim().to_owned(),
                    check_platform: gui.helper_upload_check_platform,
                    binary: gui.helper_binary.as_ref().map(std::path::PathBuf::from),
                    overwrite: gui.helper_upload_overwrite,
                    shared_dir: gui.helper_upload_shared_dir,
                },
            ));
        }
        for (source, _) in &uploads {
            if !helpers.iter().any(|helper| &helper.source == source) {
                return Err((
                    "gui.helper_upload",
                    format!("names the source `{source}`, which has no gui.source entry"),
                ));
            }
        }
        for helper in &mut helpers {
            helper.upload = uploads
                .iter()
                .find(|(source, _)| source == &helper.source)
                .map(|(_, upload)| upload.clone());
            if helper.command.is_empty() {
                let program = if helper.upload.is_some() {
                    HELPER_WORD
                } else {
                    "bohio"
                };
                helper.command = [program, "gui-helper", "--stdio"]
                    .map(str::to_owned)
                    .to_vec();
            }
            let names_it = helper.command.iter().any(|word| word.contains(HELPER_WORD));
            match (names_it, helper.upload.is_some()) {
                (true, false) => {
                    return Err((
                        "gui.source",
                        format!(
                            "the command of `{}` names {HELPER_WORD} and gui.helper_upload puts nothing on that source",
                            helper.source
                        ),
                    ));
                }
                (false, true) => {
                    return Err((
                        "gui.helper_upload",
                        format!(
                            "puts a helper on `{}` and its gui.source command never names {HELPER_WORD}",
                            helper.source
                        ),
                    ));
                }
                _ => {}
            }
        }
        Ok(helpers)
    }

    /// Reads `NAME` or `NAME=COMMAND LINE`. A name alone leaves the command empty: [`ExecHelper::list`] fills in the default,
    /// `bohio gui-helper --stdio`, or the same with `{helper}` for the program when an upload puts the helper there.
    ///
    /// # Errors
    ///
    /// Why the text is not one.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (source, line) = match text.split_once('=') {
            Some((source, line)) => (source, Some(line)),
            None => (text, None),
        };
        let source = source.trim();
        if source.is_empty() {
            return Err(format!("`{text}` names no source"));
        }
        let command = match line {
            Some(line) => {
                let words = bohio_client::dial::split_command(line)?;
                if words.is_empty() {
                    return Err(format!("`{text}` names no command"));
                }
                words
            }
            None => Vec::new(),
        };
        Ok(Self {
            source: source.to_owned(),
            upload: None,
            command,
        })
    }
}

/// The output of a process, as a reader that ends when the process closes its output.
struct Out(Arc<Object<dyn Process>>);

impl Read for Out {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.0.read(1, buffer, POLL_MS) {
                Ok(0) => {}
                Ok(count) if count > 0 => {
                    return Ok(usize::try_from(count).unwrap_or(0));
                }
                Ok(_) => return Ok(0),
                Err(error) => return Err(io::Error::other(error.message().to_string())),
            }
        }
    }
}

/// The input of a process; dropping it closes the process's input.
struct In(Arc<Object<dyn Process>>);

impl Write for In {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.0
            .write(data)
            .map(|taken| usize::try_from(taken).unwrap_or(data.len()))
            .map_err(|error| io::Error::other(error.message().to_string()))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for In {
    fn drop(&mut self) {
        let _ = self.0.close_input();
    }
}

impl Node {
    /// Serves each of `helpers` until `stop` is set: starts the helper on its source, serves it as a session of this node
    /// for as long as it runs and starts it again `retry` after it ends or could not start.
    pub fn serve_exec_helpers(&self, helpers: &[ExecHelper], retry: Duration, stop: &AtomicBool) {
        thread::scope(|scope| {
            for helper in helpers {
                scope.spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        if let Err(why) = self.serve_exec_helper(helper, stop) {
                            tracing::warn!(source = %helper.source, "the helper is not served: {why}");
                        }
                        let until = std::time::Instant::now() + retry;
                        while !stop.load(Ordering::SeqCst) && std::time::Instant::now() < until {
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                });
            }
        });
    }

    fn serve_exec_helper(&self, helper: &ExecHelper, stop: &AtomicBool) -> Result<(), String> {
        let parts = self.gui_parts();
        let source = parts
            .state
            .sources
            .exec(Some(&helper.source))
            .map_err(|error| error.message)?;
        let argv = match &helper.upload {
            Some(upload) => {
                let path =
                    helper_upload::upload(&parts.state.sources, &helper.source, upload, stop)?;
                helper
                    .command
                    .iter()
                    .map(|word| word.replace(HELPER_WORD, &path))
                    .collect()
            }
            None => helper.command.clone(),
        };
        let process = source
            .get()
            .start(ExecStart {
                argv,
                stdin: true,
                ..ExecStart::default()
            })
            .map_err(|error| error.message().to_string())?;
        tracing::debug!(source = %helper.source, "the helper started");
        let process = Arc::new(process);
        // The helper's messages to the log go to its standard error; unread, a full queue would stop its output too.
        let errors = Arc::clone(&process);
        let name = helper.source.clone();
        let done = Arc::new(AtomicBool::new(false));
        let drain = {
            let done = Arc::clone(&done);
            thread::spawn(move || {
                let mut buffer = [0_u8; 1024];
                while !done.load(Ordering::SeqCst) {
                    match errors.read(2, &mut buffer, POLL_MS) {
                        Ok(count) if count > 0 => {
                            let end = usize::try_from(count).unwrap_or(0);
                            for line in String::from_utf8_lossy(&buffer[..end]).lines() {
                                tracing::debug!(source = %name, "helper: {line}");
                            }
                        }
                        Ok(0) => {}
                        _ => break,
                    }
                }
            })
        };
        let channel = bohio_client::link::stream(
            Duplex::new(Out(Arc::clone(&process)), In(Arc::clone(&process))),
            &parts.settings,
        )
        .map_err(|error| error.to_string());
        let result = channel.map(|channel| {
            // The account the node lists the session under is the source, which is what authenticated the helper.
            let account = format!("exec:{}", helper.source);
            self.serve_helper_channel(&channel, &account, Some(stop));
            channel.close();
        });
        let ended = process.kill();
        tracing::debug!(source = %helper.source, "the helper ended: {ended:?}");
        done.store(true, Ordering::SeqCst);
        let _ = drain.join();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GuiConfig;

    fn gui(exec: &[&str], upload: &[&str]) -> GuiConfig {
        GuiConfig {
            source: exec.iter().map(|text| (*text).to_owned()).collect(),
            helper_upload: upload.iter().map(|text| (*text).to_owned()).collect(),
            ..GuiConfig::default()
        }
    }

    #[test]
    fn an_upload_belongs_to_the_command_that_names_the_helper() {
        let helpers = ExecHelper::list(&gui(
            &[
                "a={helper} gui-helper --stdio",
                "b=bohio gui-helper --stdio",
            ],
            &["a@files=/home/me/.cache"],
        ))
        .expect("a consistent configuration");
        let upload = helpers[0].upload.as_ref().expect("a has an upload");
        assert_eq!(upload.files.as_deref(), Some("files"));
        assert_eq!(upload.dir, "/home/me/.cache");
        assert!(upload.check_platform && !upload.overwrite && !upload.shared_dir);
        assert!(helpers[1].upload.is_none());
    }

    #[test]
    fn the_keys_are_source_and_source_retry_and_the_old_names_are_gone() {
        let read = |text: &str| toml::from_str::<GuiConfig>(text);
        let config = read(
            "source = [\"a\", \"b=bohio gui-helper --stdio\"]
source_retry = \"3s\"",
        )
        .expect("the new keys");
        assert_eq!(config.source, ["a", "b=bohio gui-helper --stdio"]);
        assert_eq!(config.source_retry.0, std::time::Duration::from_secs(3));
        assert!(read("helper_exec = [\"a=x\"]").is_err());
        assert!(read("helper_exec_retry = \"3s\"").is_err());
    }

    #[test]
    fn a_name_alone_starts_the_helper_by_its_default_command() {
        let helpers = ExecHelper::list(&gui(&["plain", "put"], &["put=/home/me/.cache"]))
            .expect("names alone");
        assert_eq!(helpers[0].command, ["bohio", "gui-helper", "--stdio"]);
        assert!(helpers[0].upload.is_none());
        assert_eq!(helpers[1].command, ["{helper}", "gui-helper", "--stdio"]);
        assert!(helpers[1].upload.is_some());
    }

    #[test]
    fn a_configuration_that_cannot_work_is_named_by_its_key() {
        let key =
            |exec: &[&str], upload: &[&str]| ExecHelper::list(&gui(exec, upload)).unwrap_err().0;
        assert_eq!(key(&["a={helper} x"], &[]), "gui.source");
        assert_eq!(key(&["a=bohio x"], &["a=/d"]), "gui.helper_upload");
        assert_eq!(key(&["a={helper}"], &["b=/d"]), "gui.helper_upload");
        assert_eq!(key(&["a={helper}"], &["a"]), "gui.helper_upload");
        assert_eq!(key(&["a={helper}"], &["a="]), "gui.helper_upload");
        assert_eq!(key(&["=x"], &[]), "gui.source");
        assert_eq!(key(&["a="], &[]), "gui.source");
    }
}
