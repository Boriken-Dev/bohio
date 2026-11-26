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

/// How the Windows backend captures the screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowsCapture {
    /// DXGI Desktop Duplication, and GDI for a display it cannot duplicate (a rotated one, a remote session, an adapter
    /// without it).
    Auto,
    /// DXGI Desktop Duplication only: a display it cannot duplicate is `unavailable`.
    Dxgi,
    /// GDI only: slower on a large screen, and it works wherever a screen is drawn.
    Gdi,
}

/// `[gui]`: the desktop, as a host offers it. Every permission is off by default: a peer needs `gui.view`, `gui.input` or
/// `gui.clipboard` in its own table, and the host needs `enabled = true`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct GuiConfig {
    /// Whether the host serves its desktop at all. Default `false`.
    pub enabled: bool,
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
    /// Updates a second a watch sends when it names no rate. At least 1. Default `5`.
    pub default_fps: u32,
    /// The most updates a second a watch may ask for; a request for more gets this. At least 1. Default `15`.
    pub max_fps: u32,
    /// The fewest updates a second the adaptive stream falls to. At least 1. Default `1`.
    pub min_fps: u32,
    /// The side of the tiles a frame is compared in, in pixels; a changed tile is resent. At least 1. Default `64`.
    pub tile: u32,
    /// The most updates a watch sends before the client acknowledges one; nothing queues past it. At least 1. Default `2`.
    pub frames_in_flight: u32,
    /// What a rectangle may cost, in bytes a pixel after zstd, to be sent lossless when a watch leaves the choice to the
    /// host (`auto`); one that costs more goes as JPEG. Above 0. Default `0.75`.
    pub lossless_max_bpp: f64,
    /// How long a rectangle sent as JPEG stays unchanged before it is resent lossless. Not zero. Default `1s`.
    pub refine_after: Span,
    /// The zstd level of `zstd_rgb` rectangles; the host's compression limits apply. Default `3`.
    pub zstd_level: i32,
    /// The JPEG quality a watch starts at when it names none, 1 to 100. Default `90`.
    pub watch_quality: u8,
    /// Whether the host lowers a watch's quality, size and rate when the client acknowledges updates late, and raises
    /// them when it acknowledges them early. Without it a watch keeps what it asked for and the link sets the pace.
    /// Default `true`.
    pub adapt: bool,
    /// The steps below full quality, each `QUALITY` or `QUALITY@SCALE` (a JPEG quality, 1 to 100, and a scale above 0 and
    /// at most 1); the rate halves, down to `min_fps`, past the last. Default `85`, `70`, `50`, `50@0.75`, `40@0.5`.
    pub adapt_ladder: Vec<String>,
    /// How many of the latest acknowledgements the least delay is taken from: a delay is judged by what it exceeds that
    /// least by, so the link round trip is not lateness. At least 1. Default `32`.
    pub adapt_window: usize,
    /// Consecutive late acknowledgements that step down. At least 1. Default `3`.
    pub adapt_down_after: u32,
    /// Consecutive early acknowledgements that step up. At least 1. Default `5`.
    pub adapt_up_after: u32,
    /// An acknowledgement that exceeds the window least delay by more than this many requested frame intervals is late.
    /// Above 0. Default `1.5`.
    pub adapt_late: f64,
    /// An acknowledgement that exceeds it by no more than this many requested frame intervals is early. Above 0. Default
    /// `0.5`.
    pub adapt_early: f64,
    /// The most bytes a watch's updates add up to in a second, whatever the link allows; a client may ask for less.
    /// Unset is no cap beyond the adaptive stream. Not zero.
    pub max_rate: Option<u64>,
    /// The most bytes of one rectangle, and of one update's rectangles in all, that a client holds. At least 1. Default
    /// `33554432`.
    pub max_rect_bytes: u64,
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
    /// The accounts whose per-user GUI helper this node serves (`bohio gui-helper`): Windows SIDs or names, Unix uids or
    /// names. A connection to the helper endpoint from any other account is closed unread. Empty: no helper endpoint. Read
    /// from the lowest layer only. Default empty.
    pub helper_accounts: Vec<String>,
    /// The `pipe://` endpoint helpers connect to. Windows default `pipe://bohio-gui-` and the first 16 hex digits of the
    /// node id; Unix: required when `helper_accounts` is set (a socket directory only the node account may write). Read
    /// from the lowest layer only.
    pub helper_endpoint: Option<String>,
    /// The most helper connections the node holds at once; one past it is closed unread. At least 1. Default `8`.
    pub max_helpers: usize,
    /// What `bohio gui-helper` accepts as the account that runs the node: a SID or name (Windows), a uid or name (Unix).
    /// Default `NT AUTHORITY\LocalService` (`S-1-5-19`) on Windows and `bohio` on Unix. Read from the lowest layer only.
    pub node_account: Option<String>,
    /// Helpers that run on another machine, each `SOURCE=COMMAND`: the node starts COMMAND (`bohio gui-helper --stdio`) with
    /// the exec source SOURCE (an ssh source) and serves that machine's desktop as a session of its own. What authenticates the
    /// helper is the source's login, so the machine's account is the one the source logs in as. Read from the lowest layer
    /// only. Default empty.
    pub helper_exec: Vec<String>,
    /// How long after a remote helper ends or fails to start the node starts it again. Not zero. Default `10s`.
    pub helper_exec_retry: Span,
    /// Put the node's own helper on a machine before `helper_exec` runs it, each `SOURCE=DIRECTORY` or `SOURCE@FILES=DIRECTORY`:
    /// the file goes through the files source FILES (default: SOURCE) into DIRECTORY, in that source's own syntax, and the
    /// source's `helper_exec` command names it as `{helper}`. Read from the lowest layer only. Default empty: nothing is put anywhere.
    pub helper_upload: Vec<String>,
    /// The file `helper_upload` puts: an executable built for the machine. Default the node's own executable. Read from the
    /// lowest layer only.
    pub helper_binary: Option<String>,
    /// Replace a file of the upload's name that is not the helper. Default `false`: it is refused. Lowest layer only.
    pub helper_upload_overwrite: bool,
    /// Accept an upload directory another account can write to, where it could replace the helper before it runs. Default
    /// `false`. Lowest layer only.
    pub helper_upload_shared_dir: bool,
    /// Ask the machine what it runs (`uname -sm` through the exec source) and refuse a helper built for another platform.
    /// Default `true`; `false` for a machine with no `uname`. Lowest layer only.
    pub helper_upload_check_platform: bool,
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
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
            default_fps: 5,
            max_fps: 15,
            min_fps: 1,
            tile: 64,
            frames_in_flight: 2,
            lossless_max_bpp: 0.75,
            refine_after: Span(Duration::from_secs(1)),
            zstd_level: 3,
            watch_quality: 90,
            adapt: true,
            adapt_ladder: ["85", "70", "50", "50@0.75", "40@0.5"]
                .map(str::to_owned)
                .to_vec(),
            adapt_window: 32,
            adapt_down_after: 3,
            adapt_up_after: 5,
            adapt_late: 1.5,
            adapt_early: 0.5,
            max_rate: None,
            max_rect_bytes: 32 * 1024 * 1024,
            indicator: Indicator::Notify,
            indicator_linger: Span(Duration::from_secs(30)),
            approval: ApprovalMode::None,
            approval_timeout: Span(Duration::from_secs(60)),
            approval_ttl: Span(Duration::from_secs(15 * 60)),
            helper_accounts: Vec::new(),
            helper_endpoint: None,
            max_helpers: 8,
            node_account: None,
            helper_exec: Vec::new(),
            helper_exec_retry: Span(Duration::from_secs(10)),
            helper_upload: Vec::new(),
            helper_binary: None,
            helper_upload_overwrite: false,
            helper_upload_shared_dir: false,
            helper_upload_check_platform: true,
        }
    }
}

/// One step of the adaptive ladder: a JPEG quality and the scale of the frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LadderStep {
    /// JPEG quality, 1 to 100.
    pub quality: u8,
    /// Frame pixels per display pixel, above 0 and at most 1.
    pub scale: f64,
}

impl LadderStep {
    /// Reads `QUALITY` or `QUALITY@SCALE`.
    pub fn parse(text: &str) -> Option<Self> {
        let (quality, scale) = match text.split_once('@') {
            Some((quality, scale)) => (quality, scale.trim().parse::<f64>().ok()?),
            None => (text, 1.0),
        };
        let quality: u8 = quality.trim().parse().ok()?;
        ((1..=100).contains(&quality) && scale > 0.0 && scale <= 1.0)
            .then_some(Self { quality, scale })
    }
}

impl GuiConfig {
    /// The first value out of range, as the key and what is wrong with it.
    pub(super) fn validate(&self) -> Result<(), (&'static str, &'static str)> {
        for (key, zero) in [
            ("gui.max_streams", self.max_streams == 0),
            ("gui.max_streams_per_peer", self.max_streams_per_peer == 0),
            ("gui.max_helpers", self.max_helpers == 0),
            ("gui.helper_exec_retry", self.helper_exec_retry.0.is_zero()),
            ("gui.snapshots_per_minute", self.snapshots_per_minute == 0),
            ("gui.max_snapshot_pixels", self.max_snapshot_pixels == 0),
            ("gui.max_input_rate", self.max_input_rate == 0),
            ("gui.max_batch_events", self.max_batch_events == 0),
            ("gui.max_text", self.max_text == 0),
            ("gui.clipboard_max_bytes", self.clipboard_max_bytes == 0),
            ("gui.chunk_size", self.chunk_size == 0),
            ("gui.default_fps", self.default_fps == 0),
            ("gui.max_fps", self.max_fps == 0),
            ("gui.min_fps", self.min_fps == 0),
            ("gui.tile", self.tile == 0),
            ("gui.frames_in_flight", self.frames_in_flight == 0),
            ("gui.adapt_window", self.adapt_window == 0),
            ("gui.adapt_down_after", self.adapt_down_after == 0),
            ("gui.adapt_up_after", self.adapt_up_after == 0),
            ("gui.max_rect_bytes", self.max_rect_bytes == 0),
            ("gui.max_rate", self.max_rate == Some(0)),
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
            ("gui.refine_after", self.refine_after.0.is_zero()),
        ] {
            if zero {
                return Err((key, "must not be zero"));
            }
        }
        if self.formats.is_empty() {
            return Err(("gui.formats", "must name at least one format"));
        }
        if self
            .helper_accounts
            .iter()
            .any(|account| account.trim().is_empty())
        {
            return Err(("gui.helper_accounts", "must not hold an empty account"));
        }
        if cfg!(unix) && !self.helper_accounts.is_empty() && self.helper_endpoint.is_none() {
            return Err((
                "gui.helper_endpoint",
                "is required when gui.helper_accounts is set: name a pipe:// socket in a directory only the node's account may write",
            ));
        }
        if !(1..=100).contains(&self.jpeg_quality) {
            return Err(("gui.jpeg_quality", "must be from 1 to 100"));
        }
        if !(1..=100).contains(&self.watch_quality) {
            return Err(("gui.watch_quality", "must be from 1 to 100"));
        }
        if self.default_fps > self.max_fps {
            return Err(("gui.default_fps", "must not be more than gui.max_fps"));
        }
        if self.min_fps > self.max_fps {
            return Err(("gui.min_fps", "must not be more than gui.max_fps"));
        }
        for (key, value) in [
            ("gui.lossless_max_bpp", self.lossless_max_bpp),
            ("gui.adapt_late", self.adapt_late),
            ("gui.adapt_early", self.adapt_early),
        ] {
            if value.is_nan() || value <= 0.0 {
                return Err((key, "must be above 0"));
            }
        }
        if self
            .adapt_ladder
            .iter()
            .any(|step| LadderStep::parse(step).is_none())
        {
            return Err((
                "gui.adapt_ladder",
                "each step is QUALITY or QUALITY@SCALE: a quality from 1 to 100 and a scale above 0 and at most 1",
            ));
        }
        Ok(())
    }
}
