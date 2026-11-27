//! The local enums carry the protocol's `type` key like every other tagged enum on the wire.

use bohio_proto::gui::{ApproveInput, ApproveOutput, HelperChange, HelperMessage, State};
use bohio_proto::local::PairReply;
use bohio_proto::{decode, encode};
use ciborium::Value;

fn type_of<T: serde::Serialize>(value: &T) -> String {
    let Value::Map(entries) = decode::<Value>(&encode(value).unwrap()).unwrap() else {
        panic!("a map");
    };
    entries
        .into_iter()
        .find(|(key, _)| key.as_text() == Some("type"))
        .and_then(|(_, value)| value.into_text().ok())
        .expect("a type key")
}

#[test]
fn every_local_enum_is_tagged_by_a_type_key() {
    let armed = PairReply::Armed {
        pin: "1".into(),
        lifetime_ms: 2,
    };
    assert_eq!(type_of(&armed), "armed");
    assert_eq!(
        type_of(&ApproveOutput::Decided { id: 1, found: true }),
        "decided"
    );
    assert_eq!(
        type_of(&ApproveInput::Decide {
            id: 1,
            allow: false
        }),
        "decide"
    );
    assert_eq!(type_of(&HelperChange::ClipboardRead), "clipboard_read");
    let state = State {
        active: true,
        locked: None,
        ..State::default()
    };
    assert_eq!(type_of(&HelperMessage::State { state }), "state");
}
