//! The X11 clipboard: text in a selection.
//!
//! Setting the clipboard makes this node the selection's owner, and a thread with a connection of its own answers the
//! programs that ask for it until another program takes the selection or the owner is replaced; the text lives only as
//! long as the node does (X has no clipboard manager of its own). Reading asks the owner, whoever it is, for the text
//! over a connection made for the question. A transfer that would need the incremental protocol (a text larger than one
//! request) is refused, not split.

#![forbid(unsafe_code)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

use super::x11::{X11Settings, open_connection};
use crate::desktop::GuiError;

fn failed(what: &str, error: impl std::fmt::Display) -> GuiError {
    GuiError::io(format!("{what}: {error}"))
}

/// How often a read looks for the owner's answer; `clipboard_wait` bounds the wait.
const ANSWER_POLL: Duration = Duration::from_millis(2);

/// A connection with a window of its own and the atoms the clipboard speaks.
struct Session {
    connection: RustConnection,
    window: Window,
    selection: Atom,
    utf8: Atom,
    targets: Atom,
    text: Atom,
    plain: Atom,
    incr: Atom,
    property: Atom,
}

impl Session {
    fn open(display: &str, settings: &X11Settings) -> Result<Self, GuiError> {
        let selection = settings.selection.as_str();
        let (connection, number) = open_connection(settings, display).map_err(|error| {
            GuiError::unavailable(format!(
                "cannot connect to the X display {display}: {error}"
            ))
        })?;
        let root = connection
            .setup()
            .roots
            .get(number)
            .ok_or_else(|| GuiError::unavailable("the X display has no such screen"))?
            .root;
        let window = connection
            .generate_id()
            .map_err(|error| failed("making the clipboard's window", error))?;
        connection
            .create_window(
                COPY_DEPTH_FROM_PARENT,
                window,
                root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_ONLY,
                0,
                &CreateWindowAux::new(),
            )
            .map_err(|error| failed("making the clipboard's window", error))?;
        let atom = |name: &str| -> Result<Atom, GuiError> {
            Ok(connection
                .intern_atom(false, name.as_bytes())
                .map_err(|error| failed("naming the clipboard", error))?
                .reply()
                .map_err(|error| failed("naming the clipboard", error))?
                .atom)
        };
        Ok(Self {
            selection: atom(selection)?,
            utf8: atom("UTF8_STRING")?,
            targets: atom("TARGETS")?,
            text: atom("TEXT")?,
            plain: atom("text/plain;charset=utf-8")?,
            incr: atom("INCR")?,
            property: atom("BOHIO_CLIPBOARD")?,
            connection,
            window,
        })
    }
}

/// The owner thread: its stop flag, and the thread, joined when it is dropped. The thread blocks waiting for a request,
/// so ending it sends its window a message to wake it.
struct Owner {
    stop: Arc<AtomicBool>,
    session: Arc<Session>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let wake =
            ClientMessageEvent::new(32, self.session.window, self.session.property, [0_u32; 5]);
        // An empty mask sends the event to the client that made the window: the owner thread's own connection.
        let _ = self.session.connection.send_event(
            false,
            self.session.window,
            EventMask::NO_EVENT,
            wake,
        );
        let _ = self.session.connection.flush();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The clipboard of one desktop.
#[derive(Default)]
pub(super) struct Selection {
    owner: Mutex<Option<Owner>>,
}

impl Selection {
    /// Makes `text` the clipboard.
    ///
    /// # Errors
    ///
    /// `bad_request` for text over `clipboard_max_bytes` or over what one request carries; `unavailable` when the
    /// display cannot be reached or does not give the selection.
    pub(super) fn write(
        &self,
        display: &str,
        settings: &X11Settings,
        text: &str,
    ) -> Result<(), GuiError> {
        if text.len() > settings.clipboard_max_bytes {
            return Err(GuiError::bad_request(
                "the clipboard text is longer than this host writes",
            ));
        }
        let mut held = self.owner.lock().unwrap_or_else(PoisonError::into_inner);
        // The old owner ends first: it would answer for the old text until it saw the selection change hands.
        *held = None;
        let session = Session::open(display, settings)?;
        if text.len() + 64 > session.connection.maximum_request_bytes() {
            return Err(GuiError::bad_request(
                "the clipboard text is longer than one request to the display carries",
            ));
        }
        session
            .connection
            .set_selection_owner(session.window, session.selection, CURRENT_TIME)
            .map_err(|error| failed("taking the clipboard", error))?;
        let owner = session
            .connection
            .get_selection_owner(session.selection)
            .map_err(|error| failed("taking the clipboard", error))?
            .reply()
            .map_err(|error| failed("taking the clipboard", error))?
            .owner;
        if owner != session.window {
            return Err(GuiError::unavailable(
                "another program holds the clipboard and did not give it up",
            ));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let session = Arc::new(session);
        let thread = {
            let (stop, session) = (Arc::clone(&stop), Arc::clone(&session));
            let text = text.to_owned();
            thread::Builder::new()
                .name("bohio-x11-clipboard".to_owned())
                .spawn(move || serve(&session, &text, &stop))
                .map_err(|error| failed("starting the clipboard's owner", error))?
        };
        *held = Some(Owner {
            stop,
            session,
            thread: Some(thread),
        });
        Ok(())
    }

    /// The text of the clipboard, empty when nothing owns it.
    ///
    /// # Errors
    ///
    /// `unavailable` when the owner holds no text, did not answer within `clipboard_wait`, or needs the incremental
    /// protocol; `bad_request` for text over `clipboard_max_bytes`.
    pub(super) fn read(&self, display: &str, settings: &X11Settings) -> Result<String, GuiError> {
        let session = Session::open(display, settings)?;
        let owner = session
            .connection
            .get_selection_owner(session.selection)
            .map_err(|error| failed("asking for the clipboard", error))?
            .reply()
            .map_err(|error| failed("asking for the clipboard", error))?
            .owner;
        if owner == NONE {
            return Ok(String::new());
        }
        for (target, latin1) in [(session.utf8, false), (AtomEnum::STRING.into(), true)] {
            if let Some(text) = ask(&session, target, latin1, settings)? {
                return Ok(text);
            }
        }
        Err(GuiError::unavailable("the clipboard holds no text"))
    }
}

/// Asks the owner for `target`; `None` when it cannot give that.
fn ask(
    session: &Session,
    target: Atom,
    latin1: bool,
    settings: &X11Settings,
) -> Result<Option<String>, GuiError> {
    let connection = &session.connection;
    connection
        .convert_selection(
            session.window,
            session.selection,
            target,
            session.property,
            CURRENT_TIME,
        )
        .map_err(|error| failed("asking for the clipboard", error))?;
    connection
        .flush()
        .map_err(|error| failed("asking for the clipboard", error))?;
    let start = Instant::now();
    let property = loop {
        match connection
            .poll_for_event()
            .map_err(|error| failed("asking for the clipboard", error))?
        {
            Some(Event::SelectionNotify(event)) if event.requestor == session.window => {
                break event.property;
            }
            Some(_) => {}
            None if start.elapsed() >= settings.clipboard_wait => {
                return Err(GuiError::unavailable(
                    "the program that owns the clipboard did not answer in time",
                ));
            }
            None => thread::sleep(ANSWER_POLL),
        }
    };
    if property == NONE {
        return Ok(None);
    }
    // Longer than the limit by one byte is how a text over it shows: the reply says how much is left.
    let words = u32::try_from(settings.clipboard_max_bytes / 4 + 1).unwrap_or(u32::MAX);
    let reply = connection
        .get_property(true, session.window, property, AtomEnum::ANY, 0, words)
        .map_err(|error| failed("reading the clipboard", error))?
        .reply()
        .map_err(|error| failed("reading the clipboard", error))?;
    if reply.type_ == session.incr {
        return Err(GuiError::unavailable(
            "the clipboard text is too large for one transfer, and this host does not take the incremental kind",
        ));
    }
    if reply.bytes_after > 0 || reply.value.len() > settings.clipboard_max_bytes {
        return Err(GuiError::bad_request(
            "the clipboard text is longer than this host reads",
        ));
    }
    Ok(Some(if latin1 {
        reply.value.iter().map(|byte| char::from(*byte)).collect()
    } else {
        String::from_utf8_lossy(&reply.value).into_owned()
    }))
}

/// Answers the requests for the selection until another program takes it, or `stop` is set and the thread is woken. It
/// waits for an event, so an owner nobody asks costs nothing.
fn serve(session: &Session, text: &str, stop: &AtomicBool) {
    let connection = &session.connection;
    let _ = connection.flush();
    while !stop.load(Ordering::Acquire) {
        match connection.wait_for_event().map(Some) {
            Ok(Some(Event::SelectionClear(event))) if event.selection == session.selection => {
                return;
            }
            Ok(Some(Event::SelectionRequest(request)))
                if request.selection == session.selection =>
            {
                let property = if request.property == NONE {
                    request.target
                } else {
                    request.property
                };
                let answered = answer(session, text, &request, property);
                let notify = SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: 0,
                    time: request.time,
                    requestor: request.requestor,
                    selection: request.selection,
                    target: request.target,
                    property: if answered { property } else { NONE },
                };
                let _ =
                    connection.send_event(false, request.requestor, EventMask::NO_EVENT, notify);
                let _ = connection.flush();
            }
            Ok(_) => {}
            Err(_) => return,
        }
    }
}

/// Puts the answer to one request in the requestor's property; `false` for a target this owner cannot give.
fn answer(
    session: &Session,
    text: &str,
    request: &x11rb::protocol::xproto::SelectionRequestEvent,
    property: Atom,
) -> bool {
    let connection = &session.connection;
    let target = request.target;
    if target == session.targets {
        let targets = [
            session.targets,
            session.utf8,
            session.text,
            session.plain,
            AtomEnum::STRING.into(),
        ];
        return connection
            .change_property32(
                PropMode::REPLACE,
                request.requestor,
                property,
                AtomEnum::ATOM,
                &targets,
            )
            .is_ok();
    }
    if target == session.utf8 || target == session.text || target == session.plain {
        return connection
            .change_property8(
                PropMode::REPLACE,
                request.requestor,
                property,
                session.utf8,
                text.as_bytes(),
            )
            .is_ok();
    }
    if target == Atom::from(AtomEnum::STRING) {
        // Latin-1, with `?` for what it cannot hold.
        let bytes: Vec<u8> = text
            .chars()
            .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
            .collect();
        return connection
            .change_property8(
                PropMode::REPLACE,
                request.requestor,
                property,
                AtomEnum::STRING,
                &bytes,
            )
            .is_ok();
    }
    false
}
