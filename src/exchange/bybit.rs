//! Bybit v5 linear perpetuals.
//!
//! The socket sends a full snapshot after subscribing (and again whenever
//! `u == 1`, after a service restart), then deltas whose `u` increases by
//! exactly one.

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::json::{self, fields};
use super::{Instrument, Step, Tape, apply_pairs};
use crate::book::{Book, Kind, PriceScale};

pub const WS_URL: &str = "wss://stream.bybit.com/v5/public/linear";
const REST: &str = "https://api.bybit.com";
const DEPTH: u32 = 200;

pub struct Feed {
    sym: String,
    scale: PriceScale,
    prev_u: Option<u64>,
    pub event_ms: i64,
}

#[derive(Deserialize)]
#[allow(non_snake_case)]
struct Trade<'a> {
    T: i64,
    S: &'a str,
    v: &'a str,
    p: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Ticker<'a> {
    #[serde(borrow, default)]
    mark_price: Option<&'a str>,
}

impl Feed {
    pub fn new(symbol: &str, scale: PriceScale) -> Self {
        Self {
            sym: symbol.to_string(),
            scale,
            prev_u: None,
            event_ms: 0,
        }
    }

    pub fn subscribe(&self) -> Vec<String> {
        let s = &self.sym;
        vec![format!(
            r#"{{"op":"subscribe","args":["orderbook.{DEPTH}.{s}","publicTrade.{s}","tickers.{s}"]}}"#
        )]
    }

    pub fn is_synced(&self) -> bool {
        self.prev_u.is_some()
    }

    pub fn on_text(&mut self, txt: &str, book: &mut Book, tape: &mut Tape) -> Step {
        let (mut topic, mut kind, mut ts, mut data, mut rejected) = ("", "", 0, "", false);
        for (k, v) in fields(txt) {
            match k {
                "topic" => topic = json::str(v).unwrap_or(""),
                "type" => kind = json::str(v).unwrap_or(""),
                "ts" => ts = json::i64(v).unwrap_or(0),
                "data" => data = v,
                "success" => rejected = v == "false",
                _ => {}
            }
        }
        if rejected {
            tracing::warn!(msg = txt, "bybit rejected request");
            return Step::Resync("subscription rejected");
        }
        if topic.is_empty() || data.is_empty() {
            return Step::Ignore;
        }
        if topic.starts_with("orderbook.") {
            self.on_book(kind == "snapshot", ts, data, book)
        } else if topic.starts_with("publicTrade.") {
            self.on_trades(data, tape)
        } else if topic.starts_with("tickers.") {
            match serde_json::from_str::<Ticker>(data) {
                Ok(Ticker {
                    mark_price: Some(p),
                }) => {
                    tape.mark.set_decimal(p, self.scale.decimals());
                    Step::Tape
                }
                _ => Step::Ignore,
            }
        } else {
            Step::Ignore
        }
    }

    fn on_book(&mut self, snapshot: bool, ts: i64, data: &str, book: &mut Book) -> Step {
        let (mut b, mut a, mut u) = (None, None, None);
        for (k, v) in fields(data) {
            match k {
                "b" => b = Some(v),
                "a" => a = Some(v),
                "u" => u = json::u64(v),
                _ => {}
            }
        }
        let (Some(b), Some(a), Some(u)) = (b, a, u) else {
            return Step::Resync("malformed book event");
        };
        if snapshot || u == 1 {
            book.clear();
        } else {
            match self.prev_u {
                None => return Step::Ignore, // delta before the first snapshot
                Some(p) if u != p + 1 => return Step::Resync("sequence gap"),
                _ => {}
            }
        }
        self.prev_u = Some(u);
        self.event_ms = ts;
        if !(apply_pairs(book, Kind::Bid, b) & apply_pairs(book, Kind::Ask, a)) {
            return Step::Resync("malformed level");
        }
        Step::Book
    }

    fn on_trades(&self, data: &str, tape: &mut Tape) -> Step {
        // Trades arrive oldest first; the last one is the latest print.
        let Ok(trades) = serde_json::from_str::<TradeSeq>(data) else {
            return Step::Ignore;
        };
        let Some(t) = trades.last else {
            return Step::Ignore;
        };
        match (self.scale.parse(t.p), t.v.parse()) {
            (Some(px), Ok(q)) => {
                tape.trade(px, q, t.S == "Buy", t.T);
                Step::Tape
            }
            _ => Step::Ignore,
        }
    }
}

/// Walks a trade array keeping only the last element, without collecting.
struct TradeSeq<'a> {
    last: Option<Trade<'a>>,
}

impl<'de: 'a, 'a> Deserialize<'de> for TradeSeq<'a> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<'a>(std::marker::PhantomData<&'a ()>);
        impl<'de: 'a, 'a> serde::de::Visitor<'de> for V<'a> {
            type Value = TradeSeq<'a>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("trade array")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut s: A,
            ) -> Result<Self::Value, A::Error> {
                let mut last = None;
                while let Some(t) = s.next_element::<Trade<'de>>()? {
                    last = Some(t);
                }
                Ok(TradeSeq { last })
            }
        }
        d.deserialize_seq(V(std::marker::PhantomData))
    }
}

pub async fn list_instruments(http: &reqwest::Client) -> Result<Vec<Instrument>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Resp {
        ret_code: i64,
        ret_msg: String,
        result: Option<ResultBody>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ResultBody {
        list: Vec<Item>,
        #[serde(default)]
        next_page_cursor: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Item {
        symbol: String,
        base_coin: String,
        quote_coin: String,
        status: String,
        contract_type: String,
        price_filter: PriceFilter,
        lot_size_filter: LotSizeFilter,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct LotSizeFilter {
        qty_step: String,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct PriceFilter {
        tick_size: String,
    }

    let mut out = Vec::new();
    let mut cursor = String::new();
    loop {
        let resp: Resp = http
            .get(format!("{REST}/v5/market/instruments-info"))
            .query(&[
                ("category", "linear"),
                ("limit", "1000"),
                ("cursor", cursor.as_str()),
            ])
            .send()
            .await?
            .error_for_status()?
            .json()
            .await
            .context("decoding instruments-info")?;
        if resp.ret_code != 0 {
            bail!("bybit: {}", resp.ret_msg);
        }
        let Some(r) = resp.result else { break };
        out.extend(r.list.into_iter().map(|i| Instrument {
            symbol: i.symbol,
            base: i.base_coin,
            quote: i.quote_coin,
            tick: i.price_filter.tick_size,
            lot: i.lot_size_filter.qty_step,
            status: if i.contract_type == "LinearPerpetual" {
                i.status
            } else {
                format!("{} {}", i.status, i.contract_type)
            },
        }));
        if r.next_page_cursor.is_empty() {
            break;
        }
        cursor = r.next_page_cursor;
    }
    Ok(out)
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

    fn book_msg(kind: &str, u: u64, bid: &str, qty: &str) -> String {
        format!(
            r#"{{"topic":"orderbook.200.BTCUSDT","type":"{kind}","ts":7,"data":{{"s":"BTCUSDT","b":[["{bid}","{qty}"]],"a":[["200.0","1"]],"u":{u},"seq":1}},"cts":6}}"#
        )
    }

    #[test]
    fn snapshot_then_deltas() {
        let (mut f, mut b, mut t) = feed();
        assert_eq!(
            f.on_text(&book_msg("delta", 4, "99.0", "1"), &mut b, &mut t),
            Step::Ignore
        );
        assert_eq!(
            f.on_text(&book_msg("snapshot", 5, "100.0", "1"), &mut b, &mut t),
            Step::Book
        );
        assert_eq!(
            f.on_text(&book_msg("delta", 6, "100.0", "0"), &mut b, &mut t),
            Step::Book
        );
        assert!(b.best_bid().is_none());
        assert_eq!(
            f.on_text(&book_msg("delta", 8, "99.0", "1"), &mut b, &mut t),
            Step::Resync("sequence gap")
        );
    }

    #[test]
    fn u_one_resets_book() {
        let (mut f, mut b, mut t) = feed();
        f.on_text(&book_msg("snapshot", 50, "100.0", "1"), &mut b, &mut t);
        f.on_text(&book_msg("delta", 1, "90.0", "1"), &mut b, &mut t);
        assert_eq!(b.bids.len(), 1);
        assert_eq!(b.best_bid().unwrap().px, 900);
    }

    #[test]
    fn trades_and_ticker() {
        let (mut f, mut b, mut t) = feed();
        let tr = r#"{"topic":"publicTrade.BTCUSDT","type":"snapshot","ts":1,"data":[{"T":1,"s":"BTCUSDT","S":"Sell","v":"1","p":"100.0"},{"T":2,"s":"BTCUSDT","S":"Buy","v":"0.004","p":"100.1"}]}"#;
        assert_eq!(f.on_text(tr, &mut b, &mut t), Step::Tape);
        assert_eq!((t.last_px, t.last_buy, t.last_ms), (Some(1001), true, 2));
        let tk = r#"{"topic":"tickers.BTCUSDT","type":"delta","data":{"symbol":"BTCUSDT","markPrice":"83001.01"}}"#;
        assert_eq!(f.on_text(tk, &mut b, &mut t), Step::Tape);
        assert_eq!(t.mark.as_str(), "83001.01");
        let tk = r#"{"topic":"tickers.BTCUSDT","type":"delta","data":{"symbol":"BTCUSDT","indexPrice":"1"}}"#;
        assert_eq!(f.on_text(tk, &mut b, &mut t), Step::Ignore);
    }

    #[test]
    fn acks_ignored_rejects_resync() {
        let (mut f, mut b, mut t) = feed();
        let ok = r#"{"success":true,"ret_msg":"","op":"subscribe"}"#;
        assert_eq!(f.on_text(ok, &mut b, &mut t), Step::Ignore);
        let pong = r#"{"success":true,"ret_msg":"pong","op":"ping"}"#;
        assert_eq!(f.on_text(pong, &mut b, &mut t), Step::Ignore);
        let bad = r#"{"success":false,"ret_msg":"handler not found","op":"subscribe"}"#;
        assert!(matches!(f.on_text(bad, &mut b, &mut t), Step::Resync(_)));
    }
}
