//! `[gui]`: what a host does with its desktop for a peer, and how it tells the person at the screen.

use serde::Deserialize;

use bohio_proto::gui::ImageFormat;

use super::Span;
use std::time::Duration;

/// Whether the person at the screen is told, with a desktop notification, that a peer views or controls it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Indicator {
    /// A notice when a peer's engagement starts, when it first controls and when it ends.
    Notify,
    /// Nothing: the owner of an embedding application may still ask for the engagements through its own presence.
    None,
}

/// Whether a stream waits for the person at the screen before it opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// A peer whose grant holds the permission is served at once.
    None,
    /// The stream is not opened until the person at the desk allows it, on the node's local endpoint
    /// (`bohio gui allow`) or from the notice's action, or `approval_timeout` passes and it is denied.
    Local,
}

/// `[gui]`: the desktop, as a host offers it. Every permission is off by default: a peer needs `gui.view`, `gui.input` or
/// `gui.clipboard` in its own table, and the host needs `enabled = true`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GuiConfig {
    /// Whether the host serves its desktop at all. Default `false`.
    pub enabled: bool,
    /// Which desktop backend: `auto` (the one that fits where the node runs), `windows`, `x11`, `wayland`, `macos` or
    /// `helper`. A node an application gave a desktop (`Node::with_desktop`) uses that one. Default `auto`.
    pub backend: String,
    /// The most GUI streams open at once, across peers; one past it is `busy`. At least 1. Default `4`.
    pub max_streams: usize,
    /// The most GUI streams one peer has open at once; one past it is `busy`. At least 1. Default `2`.
    pub max_streams_per_peer: usize,
    /// The most snapshots (and settles) a peer takes in a minute, across its streams; past it a request is `busy`. At least
    /// 1. Default `120`.
    pub snapshots_per_minute: u32,
    /// The most pixels a snapshot may capture, before it is scaled down; a larger region is `bad_request`. At least 1.
    /// Default `33554432` (8192 x 4096).
    pub max_snapshot_pixels: u64,
    /// The image formats a snapshot may ask for, the first being the default. At least one. Default `png`, `jpeg`, `webp`.
    pub formats: Vec<ImageFormat>,
    /// The JPEG quality when a snapshot names none, 1 to 100. Default `80`.
    pub jpeg_quality: u8,
    /// The most input events a second one stream may send; a batch that would pass it is `busy` and nothing in it is
    /// applied. At least 1. Default `1000`.
    pub max_input_rate: u32,
    /// The most events in one input frame; more is `bad_request`. At least 1. Default `256`.
    pub max_batch_events: usize,
    /// The most characters in one `text` event; more is `bad_request`. At least 1. Default `4096`.
    pub max_text: usize,
    /// The most a batch's `wait` events may add up to; more is `bad_request`. Not zero. Default `5s`.
    pub max_batch_wait: Span,
    /// Whether input is applied while the screen is locked. Default `false`: typing into a lock screen guesses the user's
    /// password.
    pub input_when_locked: bool,
    /// How often a settle captures the screen. Not zero. Default `100ms`.
    pub settle_interval: Span,
    /// How long the screen must stay unchanged to count as settled, when a request names none. Not zero. Default `300ms`.
    pub settle_quiet: Span,
    /// How long a settle waits at most, when a request names none. Not zero. Default `5s`.
    pub settle_timeout: Span,
    /// How many 64 by 64 tiles may change between two captures of a settle without counting as a change (a blinking
    /// cursor is one). Default `1`.
    pub settle_ignore_tiles: u32,
    /// The most bytes of clipboard text one request reads or writes; more is refused. At least 1. Default `1048576`.
    pub clipboard_max_bytes: usize,
    /// The most image bytes in one frame, within what the stream carries. At least 1. Default `65536`.
    pub chunk_size: usize,
    /// How often a stream looks at its connection while it waits for a request. Not zero. Default `50ms`.
    pub poll_interval: Span,
    /// How often a stream asks the desktop whether its state changed, to send a `state` frame. Not zero. Default `500ms`.
    pub state_interval: Span,
    /// How long a stream may go without a request before the host ends it and frees its slot. Not zero. Default `30m`.
    pub idle_timeout: Span,
    /// Whether the person at the screen gets a notice. Default `notify`.
    pub indicator: Indicator,
    /// How long a peer's engagement lasts after its last stream closes, so an agent that opens many short streams makes one
    /// notice. `0s` releases it at once. Default `30s`.
    pub indicator_linger: Span,
    /// Whether a stream waits for the person at the screen. Default `none`.
    pub approval: ApprovalMode,
    /// How long a stream waits for a decision before it is denied. Not zero. Default `60s`.
    pub approval_timeout: Span,
    /// How long a peer allowed on a session is not asked again. `0s` asks for every stream. Default `15m`.
    pub approval_ttl: Span,
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            backend: "auto".to_owned(),
            max_streams: 4,
            max_streams_per_peer: 2,
            snapshots_per_minute: 120,
            max_snapshot_pixels: 33_554_432,
            formats: ImageFormat::ALL.to_vec(),
            jpeg_quality: 80,
            max_input_rate: 1000,
            max_batch_events: 256,
            max_text: 4096,
            max_batch_wait: Span(Duration::from_secs(5)),
            input_when_locked: false,
            settle_interval: Span(Duration::from_millis(100)),
            settle_quiet: Span(Duration::from_millis(300)),
            settle_timeout: Span(Duration::from_secs(5)),
            settle_ignore_tiles: 1,
            clipboard_max_bytes: 1024 * 1024,
            chunk_size: 64 * 1024,
            poll_interval: Span(Duration::from_millis(50)),
            state_interval: Span(Duration::from_millis(500)),
            idle_timeout: Span(Duration::from_secs(30 * 60)),
            indicator: Indicator::Notify,
            indicator_linger: Span(Duration::from_secs(30)),
            approval: ApprovalMode::None,
            approval_timeout: Span(Duration::from_secs(60)),
            approval_ttl: Span(Duration::from_secs(15 * 60)),
        }
    }
}

/// The backends `backend` may name.
pub const BACKENDS: [&str; 6] = ["auto", "windows", "x11", "wayland", "macos", "helper"];

impl GuiConfig {
    /// The first value out of range, as the key and what is wrong with it.
    pub(super) fn validate(&self) -> Result<(), (&'static str, &'static str)> {
        if !BACKENDS.contains(&self.backend.as_str()) {
            return Err((
                "gui.backend",
                "must be auto, windows, x11, wayland, macos or helper",
            ));
        }
        for (key, zero) in [
            ("gui.max_streams", self.max_streams == 0),
            ("gui.max_streams_per_peer", self.max_streams_per_peer == 0),
            ("gui.snapshots_per_minute", self.snapshots_per_minute == 0),
            ("gui.max_snapshot_pixels", self.max_snapshot_pixels == 0),
            ("gui.max_input_rate", self.max_input_rate == 0),
            ("gui.max_batch_events", self.max_batch_events == 0),
            ("gui.max_text", self.max_text == 0),
            ("gui.clipboard_max_bytes", self.clipboard_max_bytes == 0),
            ("gui.chunk_size", self.chunk_size == 0),
        ] {
            if zero {
                return Err((key, "must be at least 1"));
            }
        }
        for (key, zero) in [
            ("gui.max_batch_wait", self.max_batch_wait.0.is_zero()),
            ("gui.settle_interval", self.settle_interval.0.is_zero()),
            ("gui.settle_quiet", self.settle_quiet.0.is_zero()),
            ("gui.settle_timeout", self.settle_timeout.0.is_zero()),
            ("gui.poll_interval", self.poll_interval.0.is_zero()),
            ("gui.state_interval", self.state_interval.0.is_zero()),
            ("gui.idle_timeout", self.idle_timeout.0.is_zero()),
            ("gui.approval_timeout", self.approval_timeout.0.is_zero()),
        ] {
            if zero {
                return Err((key, "must not be zero"));
            }
        }
        if self.formats.is_empty() {
            return Err(("gui.formats", "must name at least one format"));
        }
        if !(1..=100).contains(&self.jpeg_quality) {
            return Err(("gui.jpeg_quality", "must be from 1 to 100"));
        }
        Ok(())
    }
}
