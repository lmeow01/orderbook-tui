//! The snapshot the feed thread publishes for the UI.
//!
//! It is a fixed-size value passed through a triple buffer: the feed
//! rewrites it in place and the UI reads the latest one, with no locks and
//! no allocation on either side.

use crate::book::group::Row;
use crate::book::{Level, Ticks};
use crate::exchange::Tape;

/// Maximum ladder rows per side. Taller terminals are clamped.
pub const MAX_ROWS: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FeedState {
    #[default]
    Connecting,
    /// Connected, waiting for the first snapshot.
    Syncing,
    Live,
    /// Feed lost; the last good book is still shown.
    Stale,
}

#[derive(Clone)]
pub struct LadderView {
    /// Bumped on every publish.
    pub version: u64,
    pub state: FeedState,
    /// Why the last reconnect happened.
    pub reason: &'static str,
    /// Milliseconds until the next connect attempt, while backing off.
    pub retry_in_ms: u64,
    pub has_book: bool,
    pub group: Ticks,
    /// Rows filled per side.
    pub rows: usize,
    pub bids: [Row; MAX_ROWS],
    pub asks: [Row; MAX_ROWS],
    pub best_bid: Option<Level>,
    pub best_ask: Option<Level>,
    pub bid_levels: usize,
    pub ask_levels: usize,
    pub tape: Tape,
    pub msgs_per_sec: u32,
    /// Local receive time minus exchange event time, smoothed.
    pub latency_ms: Option<i64>,
    pub reconnects: u32,
}

impl Default for LadderView {
    fn default() -> Self {
        Self {
            version: 0,
            state: FeedState::Connecting,
            reason: "",
            retry_in_ms: 0,
            has_book: false,
            group: 1,
            rows: 0,
            bids: [Row::default(); MAX_ROWS],
            asks: [Row::default(); MAX_ROWS],
            best_bid: None,
            best_ask: None,
            bid_levels: 0,
            ask_levels: 0,
            tape: Tape::default(),
            msgs_per_sec: 0,
            latency_ms: None,
            reconnects: 0,
        }
    }
}
