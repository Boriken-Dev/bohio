//! `bohio-media/1`: virtual media, through a media source.
//!
//! [`serve`] checks the peer's grant, picks the media source and does one operation. An image named by a URL is fetched by
//! the source; an image named by a path is a file of the node that must lie under the peer's `fs_roots` (the peer also
//! needs `fs.read`), and the source reads it through the host, block by block, from the confined file and from nothing else.

#![forbid(unsafe_code)]

use std::sync::Arc;

use bohio_api::guatiao::Status;
use bohio_api::guatiao::library::{Object, ProviderError};
use bohio_api::model::MediaSlot;
use bohio_api::{BlockReader, MediaSource};
use bohio_proto::media::{Answer, Image, Op, Request, Slot, Slots};
use bohio_proto::{ErrorBody, ErrorCode};
use coqui_peer::PeerId;
use coqui_transport::Channel;

use crate::access::{Sources, code_of};
use crate::config::MediaConfig;
use crate::limit::Slots as Limit;
use crate::link::{NodeEvent, NodeEvents};
use crate::policy::{Grant, Permission};
use crate::power::within;

/// What [`serve`] needs besides the stream.
#[derive(Clone, Copy)]
pub struct Context<'a> {
    /// What this peer may do.
    pub grant: &'a Grant,
    /// The `[media]` settings.
    pub config: &'a MediaConfig,
    /// The bound on concurrent streams, shared by every peer.
    pub slots: &'a Arc<Limit>,
    /// The node's sources.
    pub sources: &'a Sources,
    /// The peer.
    pub peer: PeerId,
    /// Where the owner hears what happened.
    pub events: &'a dyn NodeEvents,
}

fn refuse(code: ErrorCode, message: &str) -> ErrorBody {
    ErrorBody::new(code, message)
}

use crate::fs::Confined;

impl BlockReader for Confined {
    fn size(&self) -> Result<u64, ProviderError> {
        Ok(Confined::size(self))
    }

    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<u64, ProviderError> {
        Confined::read_at(self, offset, dst)
            .map(|count| count as u64)
            .map_err(|_| {
                ProviderError::new(Status::GUATIAO_ERR_INTERNAL, "the image cannot be read")
            })
    }
}

fn slot_of(slot: MediaSlot, show_urls: bool) -> Slot {
    Slot {
        slot: slot.slot,
        inserted: slot.inserted,
        image: slot
            .image
            .map(|image| if show_urls { image } else { redacted(&image) }),
        size: slot.size,
    }
}

/// An image text with the user, password, query and fragment of a URL taken out (`scheme://host/path`): any peer holding
/// `media` reads it back with `status`, and what follows a `?` or an `@` is often a credential. Text that is no URL is as it was.
fn redacted(image: &str) -> String {
    let Some((scheme, rest)) = image.split_once("://") else {
        return image.to_owned();
    };
    let rest = rest.split(['?', '#']).next().unwrap_or("");
    let (authority, path) = rest.find('/').map_or((rest, ""), |at| rest.split_at(at));
    let host = authority.rsplit('@').next().unwrap_or("");
    format!("{scheme}://{host}{path}")
}

fn provider(error: &ProviderError) -> ErrorBody {
    ErrorBody::new(code_of(error.status), error.message().to_string())
}

/// Serves one `bohio-media/1` stream whose open carried `metadata`: one request, one answer.
///
/// Order of checks: decode (`bad_request`), `[media] enabled` (`unsupported`), the grant holds `media` (`denied`), the source
/// (`unsupported` when none, `not_found`, `denied` when the grant does not reach it), the request (`bad_request` for an
/// insert with no slot or no image), a path image's `fs.read` (`denied`) and root (`outside_root`, `not_found`), a free
/// slot (`busy`), then the source's own answer, or `timeout` past `action_timeout`.
///
/// # Errors
///
/// The [`ErrorBody`] sent to the peer.
pub fn serve(
    channel: &Arc<dyn Channel>,
    metadata: &[u8],
    context: &Context<'_>,
) -> Result<Slots, ErrorBody> {
    let answered = decide(metadata, context);
    let frame = match &answered {
        Ok(slots) => Answer::Slots(slots.clone()),
        Err(error) => Answer::Error(error.clone()),
    }
    .encode()
    .unwrap_or_else(|_| vec![0x05]);
    let _ = channel.send(&frame);
    answered
}

fn decide(metadata: &[u8], context: &Context<'_>) -> Result<Slots, ErrorBody> {
    let request = Request::from_metadata(metadata)
        .map_err(|_| refuse(ErrorCode::BadRequest, "the request cannot be read"))?;
    let named = request.source.clone();
    let result = run(&request, context);
    // The event names no image: a URL's query may be a credential.
    context.events.event(&NodeEvent::Media {
        peer: context.peer,
        source: named.unwrap_or_default(),
        op: request.op.name().to_owned(),
        slot: request.slot.clone(),
        outcome: match &result {
            Ok(_) => "done".to_owned(),
            Err(error) => format!("{:?}", error.code),
        },
    });
    result
}

fn run(request: &Request, context: &Context<'_>) -> Result<Slots, ErrorBody> {
    if !context.config.enabled {
        return Err(refuse(
            ErrorCode::Unsupported,
            "this host does not serve media",
        ));
    }
    if !context.grant.allows(Permission::Media) {
        return Err(refuse(ErrorCode::Denied, "this peer may not use media"));
    }
    let source = context.sources.media(request.source.as_deref())?;
    if !context.sources.permits(context.grant, source.name()) {
        return Err(refuse(
            ErrorCode::Denied,
            "this peer may not use that media source",
        ));
    }
    // What the host serves to the source is decided before a slot is held and before the source is asked.
    let image = match (&request.op, &request.image) {
        (Op::Insert, None) => {
            return Err(refuse(ErrorCode::BadRequest, "an insert names an image"));
        }
        (Op::Insert, Some(Image::Path { path })) => {
            if !context.grant.allows(Permission::FsRead) {
                return Err(refuse(
                    ErrorCode::Denied,
                    "an image of the node needs fs.read as well",
                ));
            }
            let files = context.sources.files(None)?.arc();
            let confined = crate::fs::open_confined(context.grant.fs_roots(), &files, path)?;
            let name = std::path::Path::new(path)
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
            Some(Picked::Reader(confined, name))
        }
        (Op::Insert, Some(Image::Url { url })) => Some(Picked::Url(url.clone())),
        (Op::Insert, Some(_)) => {
            return Err(refuse(
                ErrorCode::Unsupported,
                "this host cannot take that kind of image",
            ));
        }
        _ => None,
    };
    let needs_slot = |what: &str| {
        request
            .slot
            .clone()
            .ok_or_else(|| refuse(ErrorCode::BadRequest, what))
    };
    let slot = match request.op {
        Op::Status => None,
        Op::Insert => Some(needs_slot("an insert names a slot")?),
        Op::Eject => Some(needs_slot("an eject names a slot")?),
        _ => {
            return Err(refuse(
                ErrorCode::Unsupported,
                "this host does not know that operation",
            ));
        }
    };
    let permit = context.slots.acquire_owned().ok_or_else(|| {
        refuse(
            ErrorCode::Busy,
            "the host is serving as many media requests as it allows",
        )
    })?;
    let shared: Arc<dyn MediaSource> = source.arc();
    let limit = context.config.action_timeout.0;
    let op = request.op;
    let answered = within(
        limit,
        permit,
        move || -> Result<Vec<MediaSlot>, ProviderError> {
            match (op, slot, image) {
                (Op::Status, _, _) => shared.status(),
                (Op::Eject, Some(slot), _) => shared.eject(&slot).map(|slot| vec![slot]),
                (Op::Insert, Some(slot), Some(Picked::Url(url))) => {
                    shared.insert_url(&slot, &url).map(|slot| vec![slot])
                }
                (Op::Insert, Some(slot), Some(Picked::Reader(file, name))) => {
                    let reader: Object<dyn BlockReader> = file.into_object();
                    shared
                        .insert_reader(&slot, &name, reader)
                        .map(|slot| vec![slot])
                }
                _ => Err(ProviderError::new(
                    Status::GUATIAO_ERR_BAD_VALUE,
                    "not a request a source can take",
                )),
            }
        },
    )?;
    let slots = answered.map_err(|error| provider(&error))?;
    Ok(Slots {
        slots: slots
            .into_iter()
            .map(|slot| slot_of(slot, context.config.show_image_urls))
            .collect(),
    })
}

/// An image decided and, for a path, already opened inside the peer's roots.
enum Picked {
    Url(String),
    Reader(Confined, String),
}

#[cfg(test)]
mod tests {
    use super::redacted;

    #[test]
    fn a_url_loses_its_user_password_query_and_fragment_and_other_text_is_kept() {
        assert_eq!(
            redacted("https://u:p@h.invalid:8443/i/x.iso?t=s#frag"),
            "https://h.invalid:8443/i/x.iso"
        );
        assert_eq!(redacted("https://h.invalid"), "https://h.invalid");
        assert_eq!(redacted("https://tok@h.invalid?x=1"), "https://h.invalid");
        assert_eq!(redacted("installer.iso"), "installer.iso");
        assert_eq!(redacted("/srv/images/a b.iso"), "/srv/images/a b.iso");
    }
}
