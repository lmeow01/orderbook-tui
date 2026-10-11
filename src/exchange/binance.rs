//! Binance USD-M futures.
//!
//! The socket carries only deltas, so the book is bootstrapped from a REST
//! snapshot. Depth events are buffered until the snapshot arrives, then
//! replayed. Each event's `pu` must equal the previous event's `u`.

use anyhow::{Context, Result};
use serde::Deserialize;

use super::json::{self, fields};
use super::{Instrument, Step, Tape, apply_pairs};
use crate::book::{Book, Kind, PriceScale};

// Binance serves order book streams on /public and trade/mark streams on
// /market; neither path carries the other's streams.
const WS_PUBLIC: &str = "wss://fstream.binance.com/public/stream?streams=";
const WS_MARKET: &str = "wss://fstream.binance.com/market/stream?streams=";
const REST: &str = "https://fapi.binance.com";

/// Depth events buffered while the snapshot is in flight. Past this the
/// snapshot is too far behind and we start over.
const MAX_BUFFERED: usize = 2_000;

enum Sync {
    /// Waiting for the REST snapshot; raw depth events are held here.
    Buffering(Vec<String>),
    /// Snapshot applied; the next event must straddle `lid`.
    First {
        lid: u64,
    },
    Live {
        prev_u: u64,
    },
}

pub struct Feed {
    sym_lower: String,
    sym: String,
    scale: PriceScale,
    sync: Sync,
    pub event_ms: i64,
}

/// A `depthUpdate` event; level arrays stay raw until the sequence checks pass.
#[derive(Default)]
#[allow(non_snake_case)]
struct Depth<'a> {
    E: i64,
    U: u64,
    u: u64,
    pu: u64,
    b: &'a str,
    a: &'a str,
}

impl<'a> Depth<'a> {
    fn parse(s: &'a str) -> Option<Self> {
        let mut d = Depth::default();
        let mut seen = 0u8;
        for (k, v) in fields(s) {
            match k {
                "E" => d.E = json::i64(v)?,
                "U" => d.U = json::u64(v)?,
                "u" => (d.u, seen) = (json::u64(v)?, seen | 1),
                "pu" => (d.pu, seen) = (json::u64(v)?, seen | 2),
                "b" => (d.b, seen) = (v, seen | 4),
                "a" => (d.a, seen) = (v, seen | 8),
                _ => {}
            }
        }
        (seen == 15).then_some(d)
    }
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct AggTrade<'a> {
    p: &'a str,
    q: &'a str,
    T: i64,
    /// Buyer is the maker, i.e. the aggressor sold.
    m: bool,
}

#[derive(Deserialize)]
struct MarkPrice<'a> {
    p: &'a str,
}

impl Feed {
    pub fn new(symbol: &str, scale: PriceScale) -> Self {
        Self {
            sym_lower: symbol.to_ascii_lowercase(),
            sym: symbol.to_string(),
            scale,
            sync: Sync::Buffering(Vec::with_capacity(64)),
            event_ms: 0,
        }
    }

    /// Book socket first, then the trade/mark socket.
    pub fn urls(&self) -> Vec<String> {
        let s = &self.sym_lower;
        vec![
            format!("{WS_PUBLIC}{s}@depth@100ms"),
            format!("{WS_MARKET}{s}@aggTrade/{s}@markPrice@1s"),
        ]
    }

    pub fn snapshot_url(&self) -> String {
        format!("{REST}/fapi/v1/depth?symbol={}&limit=1000", self.sym)
    }

    pub fn is_synced(&self) -> bool {
        !matches!(self.sync, Sync::Buffering(_))
    }

    pub fn on_text(&mut self, txt: &str, book: &mut Book, tape: &mut Tape) -> Step {
        let (mut stream, mut data) = ("", "");
        for (k, v) in fields(txt) {
            match k {
                "stream" => stream = json::str(v).unwrap_or(""),
                "data" => data = v,
                _ => {}
            }
        }
        if data.is_empty() {
            return Step::Ignore;
        }
        let kind = stream.rsplit_once('@').map_or("", |(_, k)| k);
        match kind {
            "100ms" | "depth" => {
                if let Sync::Buffering(buf) = &mut self.sync {
                    if buf.len() >= MAX_BUFFERED {
                        return Step::Resync("snapshot took too long");
                    }
                    buf.push(data.to_owned());
                    return Step::Ignore;
                }
                self.on_depth(data, book)
            }
            "aggTrade" => {
                let Ok(t) = serde_json::from_str::<AggTrade>(data) else {
                    return Step::Ignore;
                };
                match (self.scale.parse(t.p), t.q.parse()) {
                    (Some(px), Ok(q)) => {
                        tape.trade(px, q, !t.m, t.T);
                        Step::Tape
                    }
                    _ => Step::Ignore,
                }
            }
            "1s" | "markPrice" => match serde_json::from_str::<MarkPrice>(data) {
                Ok(m) => {
                    tape.mark.set_decimal(m.p, self.scale.decimals());
                    Step::Tape
                }
                Err(_) => Step::Ignore,
            },
            _ => Step::Ignore,
        }
    }

    fn on_depth(&mut self, data: &str, book: &mut Book) -> Step {
        let Some(d) = Depth::parse(data) else {
            return Step::Resync("malformed depth event");
        };
        match self.sync {
            Sync::Buffering(_) => unreachable!("buffered before reaching here"),
            Sync::First { lid } => {
                if d.u < lid {
                    return Step::Ignore; // older than the snapshot
                }
                if d.U > lid {
                    return Step::Resync("snapshot older than first event");
                }
            }
            Sync::Live { prev_u } => {
                if d.pu != prev_u {
                    return Step::Resync("sequence gap");
                }
            }
        }
        self.sync = Sync::Live { prev_u: d.u };
        self.event_ms = d.E;
        if !(apply_pairs(book, Kind::Bid, d.b) & apply_pairs(book, Kind::Ask, d.a)) {
            return Step::Resync("malformed level");
        }
        Step::Book
    }

    pub fn on_snapshot(&mut self, body: &str, book: &mut Book) -> Step {
        let (mut lid, mut bids, mut asks) = (None, None, None);
        for (k, v) in fields(body) {
            match k {
                "lastUpdateId" => lid = json::u64(v),
                "bids" => bids = Some(v),
                "asks" => asks = Some(v),
                _ => {}
            }
        }
        let (Some(lid), Some(bids), Some(asks)) = (lid, bids, asks) else {
            return Step::Resync("malformed snapshot");
        };
        book.clear();
        if !(apply_pairs(book, Kind::Bid, bids) & apply_pairs(book, Kind::Ask, asks)) {
            return Step::Resync("malformed snapshot level");
        }
        let buffered = match std::mem::replace(&mut self.sync, Sync::First { lid }) {
            Sync::Buffering(b) => b,
            _ => Vec::new(),
        };
        for ev in &buffered {
            if let Step::Resync(r) = self.on_depth(ev, book) {
                return Step::Resync(r);
            }
        }
        book.uncross();
        Step::Book
    }
}

pub async fn list_instruments(http: &reqwest::Client) -> Result<Vec<Instrument>> {
    #[derive(Deserialize)]
    struct Info {
        symbols: Vec<Sym>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Sym {
        symbol: String,
        base_asset: String,
        quote_asset: String,
        status: String,
        contract_type: String,
        filters: Vec<serde_json::Value>,
    }
    let info: Info = http
        .get(format!("{REST}/fapi/v1/exchangeInfo"))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("decoding exchangeInfo")?;
    Ok(info
        .symbols
        .into_iter()
        .filter_map(|s| {
            let filter = |kind: &str, key: &str| {
                s.filters
                    .iter()
                    .find(|f| f["filterType"] == kind)?
                    .get(key)?
                    .as_str()
                    .map(str::to_string)
            };
            let tick = filter("PRICE_FILTER", "tickSize")?;
            let lot = filter("LOT_SIZE", "stepSize").unwrap_or_else(|| "0.001".into());
            let status = if s.contract_type == "PERPETUAL" {
                s.status
            } else {
                format!("{} {}", s.status, s.contract_type)
            };
            Some(Instrument {
                symbol: s.symbol,
                base: s.base_asset,
                quote: s.quote_asset,
                tick,
                lot,
                status,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed() -> (Feed, Book, Tape) {
        let scale = PriceScale::from_tick("0.10").unwrap();
        (
            Feed::new("BTCUSDT", scale),
            Book::new(scale),
            Tape::default(),
        )
    }

    fn depth(big_u: u64, u: u64, pu: u64, bid: &str) -> String {
        format!(
            r#"{{"stream":"btcusdt@depth@100ms","data":{{"e":"depthUpdate","E":1,"U":{big_u},"u":{u},"pu":{pu},"b":[["{bid}","1.0"]],"a":[]}}}}"#
        )
    }

    const SNAP: &str = r#"{"lastUpdateId":100,"bids":[["99.0","1"]],"asks":[["101.0","1"]]}"#;

    #[test]
    fn buffers_then_syncs() {
        let (mut f, mut b, mut t) = feed();
        assert_eq!(
            f.on_text(&depth(90, 95, 89, "98.0"), &mut b, &mut t),
            Step::Ignore
        );
        assert_eq!(
            f.on_text(&depth(96, 105, 95, "99.5"), &mut b, &mut t),
            Step::Ignore
        );
        assert!(!f.is_synced());
        assert_eq!(f.on_snapshot(SNAP, &mut b), Step::Book);
        assert!(f.is_synced());
        // The u=95 event predates the snapshot and was dropped; u=105 applied.
        assert_eq!(b.best_bid().unwrap().px, 995);
        assert_eq!(b.bids.len(), 2);
        assert_eq!(
            f.on_text(&depth(106, 110, 105, "99.6"), &mut b, &mut t),
            Step::Book
        );
    }

    #[test]
    fn gap_forces_resync() {
        let (mut f, mut b, mut t) = feed();
        f.on_snapshot(SNAP, &mut b);
        assert_eq!(
            f.on_text(&depth(99, 101, 98, "99.1"), &mut b, &mut t),
            Step::Book
        );
        assert_eq!(
            f.on_text(&depth(103, 104, 102, "99.2"), &mut b, &mut t),
            Step::Resync("sequence gap")
        );
    }

    #[test]
    fn stale_snapshot_detected() {
        let (mut f, mut b, mut t) = feed();
        f.on_text(&depth(120, 130, 119, "98.0"), &mut b, &mut t);
        assert_eq!(
            f.on_snapshot(SNAP, &mut b),
            Step::Resync("snapshot older than first event")
        );
    }

    #[test]
    fn trades_and_mark() {
        let (mut f, mut b, mut t) = feed();
        let tr = r#"{"stream":"btcusdt@aggTrade","data":{"e":"aggTrade","p":"82996.30","q":"0.015","T":5,"m":true}}"#;
        assert_eq!(f.on_text(tr, &mut b, &mut t), Step::Tape);
        assert_eq!(t.last_px, Some(829963));
        assert!(!t.last_buy);
        let mk = r#"{"stream":"btcusdt@markPrice@1s","data":{"e":"markPriceUpdate","p":"82996.30000000"}}"#;
        assert_eq!(f.on_text(mk, &mut b, &mut t), Step::Tape);
        assert_eq!(t.mark.as_str(), "82996.3");
    }
}
