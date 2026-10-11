//! Exchange connectors.
//!
//! Each venue turns raw WebSocket text into book and tape updates. Messages
//! are parsed with borrowed `&str` fields, and price-level arrays are walked
//! by a seed visitor that writes each pair straight into the book, so the
//! steady-state path allocates nothing.

pub mod binance;
pub mod bybit;
pub mod coinbase;
pub mod json;

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::de::{DeserializeSeed, SeqAccess, Visitor};

use crate::book::price::InlineStr;
use crate::book::{Book, Kind, PriceScale, Ticks};

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Exchange {
    /// Binance USD-M futures
    #[value(name = "binance-futures", alias = "binance")]
    BinanceFutures,
    /// Bybit linear perpetuals (v5)
    Bybit,
    /// Coinbase Advanced Trade spot
    Coinbase,
}

impl Exchange {
    pub fn name(self) -> &'static str {
        match self {
            Self::BinanceFutures => "binance-futures",
            Self::Bybit => "bybit",
            Self::Coinbase => "coinbase",
        }
    }

    /// Normalises user input to the venue's symbol format.
    pub fn normalize_symbol(self, s: &str) -> String {
        let s = s.trim().to_ascii_uppercase();
        match self {
            Self::Coinbase => s.replace(['/', '_'], "-"),
            _ => s.replace(['/', '-', '_'], ""),
        }
    }
}

/// A tradable instrument, as listed by `ob symbols`.
#[derive(Debug, Clone)]
pub struct Instrument {
    pub symbol: String,
    pub base: String,
    pub quote: String,
    pub tick: String,
    /// Quantity step, e.g. `"0.001"`.
    pub lot: String,
    pub status: String,
}

pub async fn list_instruments(http: &reqwest::Client, ex: Exchange) -> Result<Vec<Instrument>> {
    let mut v = match ex {
        Exchange::BinanceFutures => binance::list_instruments(http).await,
        Exchange::Bybit => bybit::list_instruments(http).await,
        Exchange::Coinbase => coinbase::list_instruments(http).await,
    }
    .with_context(|| format!("listing {} symbols", ex.name()))?;
    v.sort_by(|a, b| a.symbol.cmp(&b.symbol));
    Ok(v)
}

/// Looks up one instrument, suggesting near matches when it isn't found.
pub async fn find_instrument(
    http: &reqwest::Client,
    ex: Exchange,
    symbol: &str,
) -> Result<Instrument> {
    let all = list_instruments(http, ex).await?;
    if let Some(i) = all.iter().find(|i| i.symbol == symbol) {
        return Ok(i.clone());
    }
    let mut scored: Vec<_> = all
        .iter()
        .map(|i| (similarity(symbol, &i.symbol), &i.symbol))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    let near: Vec<_> = scored
        .iter()
        .take(5)
        .filter(|s| s.0 > 0)
        .map(|s| s.1.as_str())
        .collect();
    if near.is_empty() {
        bail!(
            "unknown {} symbol {symbol:?}; try `ob symbols {}`",
            ex.name(),
            ex.name()
        );
    }
    bail!(
        "unknown {} symbol {symbol:?}; did you mean: {}?",
        ex.name(),
        near.join(", ")
    );
}

/// Cheap similarity score: shared prefix length, plus a bonus for substring hits.
fn similarity(a: &str, b: &str) -> usize {
    let prefix = a.bytes().zip(b.bytes()).take_while(|(x, y)| x == y).count();
    let sub = if b.contains(a) || a.contains(b) {
        a.len().min(b.len())
    } else {
        0
    };
    prefix * 2 + sub
}

/// Last trade and mark price.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tape {
    pub last_px: Option<Ticks>,
    pub last_qty: f64,
    /// Aggressor side of the last trade.
    pub last_buy: bool,
    /// Direction of the last price change: >0 up, <0 down.
    pub tick_dir: i8,
    pub last_ms: i64,
    /// Mark price string, kept at the exchange's own precision.
    pub mark: InlineStr,
}

impl Tape {
    pub fn trade(&mut self, px: Ticks, qty: f64, buy: bool, ts_ms: i64) {
        if let Some(prev) = self.last_px
            && px != prev
        {
            self.tick_dir = if px > prev { 1 } else { -1 };
        }
        self.last_px = Some(px);
        self.last_qty = qty;
        self.last_buy = buy;
        self.last_ms = ts_ms;
    }
}

/// What a message did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// The book changed.
    Book,
    /// Trade, mark price or other display state changed.
    Tape,
    /// Nothing visible changed (acks, heartbeats, buffered events).
    Ignore,
    /// The book can no longer be trusted. The supervisor will resync.
    Resync(&'static str),
}

/// Per-connection protocol state for one venue.
pub enum Venue {
    Binance(binance::Feed),
    Bybit(bybit::Feed),
    Coinbase(coinbase::Feed),
}

impl Venue {
    pub fn new(ex: Exchange, symbol: &str, scale: PriceScale) -> Self {
        match ex {
            Exchange::BinanceFutures => Self::Binance(binance::Feed::new(symbol, scale)),
            Exchange::Bybit => Self::Bybit(bybit::Feed::new(symbol, scale)),
            Exchange::Coinbase => Self::Coinbase(coinbase::Feed::new(symbol, scale)),
        }
    }

    /// Sockets to open. The first carries the book; venues that split
    /// trades onto another endpoint list it second.
    pub fn urls(&self) -> Vec<String> {
        match self {
            Self::Binance(f) => f.urls(),
            Self::Bybit(_) => vec![bybit::WS_URL.into()],
            Self::Coinbase(_) => vec![coinbase::WS_URL.into()],
        }
    }

    /// Messages to send on the first socket right after connecting.
    pub fn subscribe(&self) -> Vec<String> {
        match self {
            Self::Binance(_) => Vec::new(),
            Self::Bybit(f) => f.subscribe(),
            Self::Coinbase(f) => f.subscribe(),
        }
    }

    /// Application-level ping to send on an interval, if the venue needs one.
    pub fn ping(&self) -> Option<(Duration, &'static str)> {
        match self {
            Self::Bybit(_) => Some((Duration::from_secs(20), r#"{"op":"ping"}"#)),
            _ => None,
        }
    }

    /// Reconnect if nothing at all arrives for this long.
    pub fn idle_timeout(&self) -> Duration {
        match self {
            // Heartbeats arrive every second.
            Self::Coinbase(_) => Duration::from_secs(10),
            // Depth pushes every 100ms and mark price every second.
            Self::Binance(_) => Duration::from_secs(10),
            // Pongs every 20s at worst.
            Self::Bybit(_) => Duration::from_secs(30),
        }
    }

    /// Rotate the connection before the venue cuts it.
    pub fn max_age(&self) -> Option<Duration> {
        match self {
            Self::Binance(_) => Some(Duration::from_secs(23 * 3600)),
            _ => None,
        }
    }

    /// REST snapshot to fetch after connecting, if the venue doesn't send one on the socket.
    pub fn snapshot_url(&self) -> Option<String> {
        match self {
            Self::Binance(f) => Some(f.snapshot_url()),
            _ => None,
        }
    }

    pub fn on_snapshot(&mut self, body: &str, book: &mut Book) -> Step {
        match self {
            Self::Binance(f) => f.on_snapshot(body, book),
            _ => Step::Ignore,
        }
    }

    #[inline]
    pub fn on_text(&mut self, txt: &str, book: &mut Book, tape: &mut Tape) -> Step {
        let step = match self {
            Self::Binance(f) => f.on_text(txt, book, tape),
            Self::Bybit(f) => f.on_text(txt, book, tape),
            Self::Coinbase(f) => f.on_text(txt, book, tape),
        };
        if step == Step::Book {
            book.uncross();
        }
        step
    }

    pub fn is_synced(&self) -> bool {
        match self {
            Self::Binance(f) => f.is_synced(),
            Self::Bybit(f) => f.is_synced(),
            Self::Coinbase(f) => f.is_synced(),
        }
    }

    /// Exchange timestamp of the latest book event, in Unix ms.
    pub fn event_ms(&self) -> i64 {
        match self {
            Self::Binance(f) => f.event_ms,
            Self::Bybit(f) => f.event_ms,
            Self::Coinbase(f) => f.event_ms,
        }
    }
}

/// Applies a raw JSON array of `["price","qty"]` pairs to one side of the
/// book. Returns `false` if any element was malformed.
pub(crate) fn apply_pairs(book: &mut Book, kind: Kind, raw: &str) -> bool {
    let mut de = serde_json::Deserializer::from_str(raw);
    PairsSeed { book, kind }
        .deserialize(&mut de)
        .unwrap_or(false)
}

struct PairsSeed<'b> {
    book: &'b mut Book,
    kind: Kind,
}

impl<'de> DeserializeSeed<'de> for PairsSeed<'_> {
    type Value = bool;
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<bool, D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de> Visitor<'de> for PairsSeed<'_> {
    type Value = bool;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("an array of [price, qty] pairs")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<bool, A::Error> {
        let mut ok = true;
        while let Some((p, q)) = seq.next_element::<(&'de str, &'de str)>()? {
            ok &= self.book.set_str(self.kind, p, q);
        }
        Ok(ok)
    }
}

/// Parses an RFC 3339 UTC timestamp like `2026-10-10T18:22:17.320496592Z`
/// into Unix milliseconds without allocating.
pub(crate) fn rfc3339_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| -> Option<i64> {
        let mut v = 0i64;
        for &c in b.get(r)? {
            let d = c.wrapping_sub(b'0');
            if d > 9 {
                return None;
            }
            v = v * 10 + d as i64;
        }
        Some(v)
    };
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut ms = 0;
    if b.get(19) == Some(&b'.') {
        let mut scale = 100;
        for &c in &b[20..] {
            let dg = c.wrapping_sub(b'0');
            if dg > 9 {
                break;
            }
            ms += dg as i64 * scale;
            scale /= 10;
        }
    }
    // Days from civil, Howard Hinnant's algorithm.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((mo + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + h) * 60 + mi) * 60_000 + sec * 1000 + ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339() {
        assert_eq!(rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            rfc3339_ms("2026-10-10T18:22:17.320496592Z"),
            Some(1_791_656_537_320)
        );
        assert_eq!(rfc3339_ms("2000-02-29T12:00:00.5Z"), Some(951_825_600_500));
        assert_eq!(rfc3339_ms("garbage"), None);
    }

    #[test]
    fn pairs_apply() {
        let mut b = Book::new(PriceScale::from_tick("0.1").unwrap());
        assert!(apply_pairs(
            &mut b,
            Kind::Bid,
            r#"[["100.1","2"],["100.0","0.5"]]"#
        ));
        assert_eq!(b.best_bid().unwrap().px, 1001);
        assert_eq!(b.bids.len(), 2);
    }

    #[test]
    fn symbol_normalisation() {
        assert_eq!(
            Exchange::BinanceFutures.normalize_symbol("btc/usdt"),
            "BTCUSDT"
        );
        assert_eq!(Exchange::Coinbase.normalize_symbol("btc_usd"), "BTC-USD");
    }
}
