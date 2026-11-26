//! What the Windows session's answers mean, and the clipboard's text encoding: pure, so every rule is tested on every
//! platform. The Windows backend asks the operating system and hands the answers here.

#![forbid(unsafe_code)]

use crate::gui::GuiError;

/// What a node says when it runs in session 0.
pub(crate) const SESSION_ZERO: &str =
    "session 0 has no desktop: run the node in the user's session, or use the GUI helper";

/// What a request that needs the screen's input says when the secure desktop has it.
pub(crate) const SECURE_DESKTOP: &str = "the secure desktop has the screen (the lock screen, a sign-in or an elevation \
     prompt), which a process in the user's session can neither see nor drive";

/// Whether the session is at the screen, as the terminal service reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Connection {
    /// Connected and in use: the console, or a remote session someone is looking at.
    Active,
    /// Any other state: disconnected (a remote session closed, the console switched to another user), shadowing, idle.
    Away,
}

/// What the operating system said of the session and its input desktop.
#[derive(Debug, Clone)]
pub(crate) struct Facts {
    /// `ProcessIdToSessionId` of this process.
    pub session: u32,
    /// `WTSConnectState`, or `None` when it could not be read.
    pub connection: Option<Connection>,
    /// The input desktop's name (`UOI_NAME`), or `None` when it could not be opened: a process in a user session is
    /// refused the secure desktop's, which is how a lock screen shows.
    pub input_desktop: Option<String>,
}

/// The state a desktop reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Judged {
    pub active: bool,
    pub locked: bool,
}

/// The state of the desktop, or why there is none.
///
/// # Errors
///
/// `unavailable` for session 0.
pub(crate) fn judge(facts: &Facts) -> Result<Judged, GuiError> {
    if facts.session == 0 {
        return Err(GuiError::unavailable(SESSION_ZERO));
    }
    Ok(Judged {
        // A connection state that could not be read is not a reason to refuse: the input desktop says the rest.
        active: facts.connection != Some(Connection::Away),
        locked: facts
            .input_desktop
            .as_deref()
            .is_none_or(|name| !name.eq_ignore_ascii_case("Default")),
    })
}

/// The characters of the clipboard's `CF_UNICODETEXT` up to the first NUL, with line breaks as the wire has them (a bare
/// LF): Windows programs put CR LF.
pub(crate) fn text_of_clipboard(units: &[u16]) -> String {
    let end = units
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end]).replace("\r\n", "\n")
}

/// `text` as `CF_UNICODETEXT` holds it: UTF-16 with CR LF line breaks and a final NUL.
///
/// # Errors
///
/// `bad_request` for text that holds a NUL, which would end it early.
pub(crate) fn clipboard_of_text(text: &str) -> Result<Vec<u16>, GuiError> {
    if text.contains('\0') {
        return Err(GuiError::bad_request(
            "clipboard text cannot hold a NUL character",
        ));
    }
    let mut units = Vec::with_capacity(text.len() + 1);
    let mut previous = '\0';
    for character in text.chars() {
        if character == '\n' && previous != '\r' {
            units.push(u16::from(b'\r'));
        }
        let mut buffer = [0_u16; 2];
        units.extend_from_slice(character.encode_utf16(&mut buffer));
        previous = character;
    }
    units.push(0);
    Ok(units)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(session: u32, connection: Option<Connection>, desktop: Option<&str>) -> Facts {
        Facts {
            session,
            connection,
            input_desktop: desktop.map(str::to_owned),
        }
    }

    #[test]
    fn session_zero_has_no_desktop_and_says_what_to_do() {
        let error = judge(&facts(0, Some(Connection::Active), Some("Default"))).unwrap_err();
        assert_eq!(error.code, bohio_proto::ErrorCode::Unavailable);
        assert!(error.message.contains("user's session"), "{error}");
        assert!(error.message.contains("GUI helper"), "{error}");
    }

    #[test]
    fn the_default_desktop_is_unlocked_and_any_other_is_locked() {
        let judged = judge(&facts(1, Some(Connection::Active), Some("Default"))).unwrap();
        assert_eq!(
            judged,
            Judged {
                active: true,
                locked: false
            }
        );
        let judged = judge(&facts(2, Some(Connection::Active), Some("default"))).unwrap();
        assert!(!judged.locked);
        for name in ["Winlogon", "Screen-saver", "Disconnect"] {
            let judged = judge(&facts(1, Some(Connection::Active), Some(name))).unwrap();
            assert!(judged.locked, "{name}");
        }
    }

    #[test]
    fn an_input_desktop_that_cannot_be_opened_is_the_secure_one() {
        let judged = judge(&facts(1, Some(Connection::Active), None)).unwrap();
        assert!(judged.locked);
    }

    #[test]
    fn a_session_switched_away_is_not_active_and_an_unreadable_state_is() {
        assert!(
            !judge(&facts(3, Some(Connection::Away), Some("Default")))
                .unwrap()
                .active
        );
        assert!(judge(&facts(3, None, Some("Default"))).unwrap().active);
    }

    #[test]
    fn clipboard_text_round_trips_and_uses_crlf_inside() {
        let units = clipboard_of_text("a\nb\r\nc é 😀").unwrap();
        assert_eq!(units.last(), Some(&0));
        let text = String::from_utf16(&units[..units.len() - 1]).unwrap();
        assert_eq!(text, "a\r\nb\r\nc é 😀");
        assert_eq!(text_of_clipboard(&units), "a\nb\nc é 😀");
    }

    #[test]
    fn clipboard_text_stops_at_the_first_nul_and_refuses_one() {
        let units: Vec<u16> = "ab\0cd".encode_utf16().collect();
        assert_eq!(text_of_clipboard(&units), "ab");
        assert_eq!(text_of_clipboard(&[]), "");
        let error = clipboard_of_text("a\0b").unwrap_err();
        assert_eq!(error.code, bohio_proto::ErrorCode::BadRequest);
    }
}
