//! Detached sessions: attaching to one without knowing whether it is an exec or a shell (`bohio-attach/1`).
//!
//! A session is started by a request with a `session` ([`Exec::start`] or [`Shell::start`]); its id comes from
//! [`Exec::session`] or [`Shell::session`], and [`Control::sessions`](crate::control::Control::sessions) lists the ones a
//! peer holds. [`Exec::attach`] and [`Shell::attach`] attach when the kind is known.

use std::time::Duration;

use bohio_proto::session::{Attached, Kind};

use crate::exec::{Exec, attach_channel};
use crate::shell::Shell;
use crate::{Error, Open};

/// A stream attached to a session, of the kind the session is.
#[derive(Debug)]
#[non_exhaustive]
pub enum Attachment {
    /// An exec: standard output and standard error apart.
    Exec(Box<Exec>),
    /// A shell: one terminal.
    Shell(Shell),
}

/// Attaches to the session `session` on `link`, reading its output from the offset `from` (`0` for all that is still kept,
/// `None` for only what comes next). The [`Attached`] says where the output starts and how much of what was asked for the
/// host had already forgotten.
///
/// # Errors
///
/// [`Error::Remote`] when the host refused (`not_found` for a session this peer does not hold, `denied`, `bad_request` for an
/// offset not written yet), [`Error::Protocol`] for a kind this version does not know, else whatever `link` reports.
pub fn attach(
    link: &dyn Open,
    session: &str,
    from: Option<u64>,
) -> Result<(Attachment, Attached), Error> {
    attach_within(link, session, from, Some(crate::DEFAULT_TIMEOUT))
}

/// As [`attach`], waiting at most `timeout` for the host to answer (`None` waits without limit; [`attach`] takes
/// [`crate::DEFAULT_TIMEOUT`]).
///
/// # Errors
///
/// As [`attach`]; [`Error::Timeout`] when the host did not answer in time.
pub fn attach_within(
    link: &dyn Open,
    session: &str,
    from: Option<u64>,
    timeout: Option<Duration>,
) -> Result<(Attachment, Attached), Error> {
    let (channel, attached) = attach_channel(link, session, from, timeout)?;
    let attachment = match attached.kind {
        Kind::Exec => Attachment::Exec(Box::new(Exec::attached_to(channel, session))),
        Kind::Shell => Attachment::Shell(Shell::attached_to(channel, session)),
        _ => {
            channel.close();
            return Err(Error::Protocol(bohio_proto::Error::Decode));
        }
    };
    Ok((attachment, attached))
}
