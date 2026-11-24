//! Whether the login session is locked, from logind's `LockedHint` on the system bus.
//!
//! A host with no logind (a container, a minimal system, a bus that does not answer) has no lock detection: the answer
//! is `None`, never a guess, and the desktop says so in `info`.

#![forbid(unsafe_code)]

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use zbus::{Connection, OwnedObjectPath, Proxy, connection::Builder};

const NAME: &str = "org.freedesktop.login1";
const MANAGER: &str = "/org/freedesktop/login1";

/// What logind says of a session. Its `Active` property is not asked: it describes a seat, which an X display need not have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Hints {
    /// `LockedHint`: the session's screen is locked.
    pub locked: bool,
}

/// A way to ask logind about one session, keeping its bus connection between questions.
pub(crate) struct Logind {
    /// The system bus's address; `None` is the standard one.
    bus: Option<String>,
    /// The session's id (`XDG_SESSION_ID`); `None` asks for the session of this process.
    session: Option<String>,
    /// How long a call may wait for an answer.
    timeout: Duration,
    connection: Mutex<Option<Connection>>,
}

impl Logind {
    pub(crate) fn new(bus: Option<String>, session: Option<String>, timeout: Duration) -> Self {
        Self {
            bus,
            session,
            timeout,
            connection: Mutex::new(None),
        }
    }

    /// The session's hints, or `None` when logind cannot be reached or does not know the session.
    pub(crate) fn hints(&self) -> Option<Hints> {
        let mut held = self
            .connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let asked = zbus::block_on(async {
            if held.is_none() {
                let builder = match &self.bus {
                    Some(address) => Builder::address(address.as_str()),
                    None => Builder::system(),
                };
                *held = Some(builder.method_timeout(self.timeout).build().await.ok()?);
            }
            let connection = held.as_ref()?;
            let manager = Proxy::new(connection, NAME, MANAGER, "org.freedesktop.login1.Manager")
                .await
                .ok()?;
            let path: OwnedObjectPath = match &self.session {
                Some(id) => manager.call("GetSession", &(id.as_str(),)).await.ok()?,
                None => manager
                    .call("GetSessionByPID", &(std::process::id(),))
                    .await
                    .ok()?,
            };
            let session = Proxy::new(
                connection,
                NAME,
                path.as_ref(),
                "org.freedesktop.login1.Session",
            )
            .await
            .ok()?;
            let locked: bool = session.get_property("LockedHint").await.ok()?;
            Some(Hints { locked })
        });
        if asked.is_none() {
            // The bus may have gone away: connect again at the next question.
            *held = None;
        }
        asked
    }
}
