//! Coinbase Advanced Trade (spot).
//!
//! `level2` sends one full-book snapshot and then updates. `sequence_num`
//! counts every message on the connection across all channels, so any jump
//! means something was lost. The `heartbeats` channel keeps quiet
//! connections alive.

use anyhow::{Context, Result};
use serde::Deserialize;
use serde::de::{DeserializeSeed, IgnoredAny, MapAccess, SeqAccess, Visitor};

use super::json::{self, fields};
use super::{Instrument, Step, Tape, rfc3339_ms};
use crate::book::{Book, Kind, PriceScale};

pub const WS_URL: &str = "wss://advanced-trade-ws.coinbase.com";
const REST: &str = "https://api.coinbase.com";

pub struct Feed {
    sym: String,
    scale: PriceScale,
    prev_seq: Option<u64>,
    synced: bool,
    pub event_ms: i64,
}

#[derive(Deserialize)]
struct Trade<'a> {
    price: &'a str,
    size: &'a str,
    /// Maker side: a `BUY` maker means the aggressor sold.
    side: &'a str,
    time: &'a str,
}

/// Newest trade in a `market_trades` events array, found without collecting.
/// Trades are listed newest first within each event and events run oldest
/// to newest, so it is the first trade of the last non-empty event.
struct NewestTrade<'a>(Option<Trade<'a>>);

impl<'de: 'a, 'a> Deserialize<'de> for NewestTrade<'a> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Events<'a>(std::marker::PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for Events<'a> {
            type Value = NewestTrade<'a>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("trade events")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Self::Value, A::Error> {
                let mut newest = None;
                while let Some(ev) = s.next_element::<Event<'de>>()? {
                    if ev.0.is_some() {
                        newest = ev.0;
                    }
                }
                Ok(NewestTrade(newest))
            }
        }
        d.deserialize_seq(Events(std::marker::PhantomData))
    }
}

/// One trade event, keeping only its first (newest) trade.
struct Event<'a>(Option<Trade<'a>>);

impl<'de: 'a, 'a> Deserialize<'de> for Event<'a> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Ev<'a>(std::marker::PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for Ev<'a> {
            type Value = Event<'a>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("trade event")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut m: M) -> Result<Self::Value, M::Error> {
                let mut first = None;
                while let Some(k) = m.next_key::<&'de str>()? {
                    if k == "trades" {
                        first = m.next_value::<First<'de>>()?.0;
                    } else {
                        m.next_value::<IgnoredAny>()?;
                    }
                }
                Ok(Event(first))
            }
        }
        d.deserialize_map(Ev(std::marker::PhantomData))
    }
}

/// First element of a trade array; the rest are skipped.
struct First<'a>(Option<Trade<'a>>);

impl<'de: 'a, 'a> Deserialize<'de> for First<'a> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V<'a>(std::marker::PhantomData<&'a ()>);
        impl<'de: 'a, 'a> Visitor<'de> for V<'a> {
            type Value = First<'a>;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("trade array")
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Self::Value, A::Error> {
                let first = s.next_element::<Trade<'de>>()?;
                while s.next_element::<IgnoredAny>()?.is_some() {}
                Ok(First(first))
            }
        }
        d.deserialize_seq(V(std::marker::PhantomData))
    }
}

impl Feed {
    pub fn new(symbol: &str, scale: PriceScale) -> Self {
        Self {
            sym: symbol.to_string(),
            scale,
            prev_seq: None,
            synced: false,
            event_ms: 0,
        }
    }

    pub fn subscribe(&self) -> Vec<String> {
        let s = &self.sym;
        vec![
            r#"{"type":"subscribe","channel":"heartbeats"}"#.into(),
            format!(r#"{{"type":"subscribe","product_ids":["{s}"],"channel":"level2"}}"#),
            format!(r#"{{"type":"subscribe","product_ids":["{s}"],"channel":"market_trades"}}"#),
        ]
    }

    pub fn is_synced(&self) -> bool {
        self.synced
    }

    pub fn on_text(&mut self, txt: &str, book: &mut Book, tape: &mut Tape) -> Step {
        let (mut channel, mut timestamp, mut seq, mut events, mut error) =
            ("", "", None, "", false);
        for (k, v) in fields(txt) {
            match k {
                "channel" => channel = json::str(v).unwrap_or(""),
                "timestamp" => timestamp = json::str(v).unwrap_or(""),
                "sequence_num" => seq = json::u64(v),
                "events" => events = v,
                "type" => error = v == r#""error""#,
                _ => {}
            }
        }
        if error {
            tracing::warn!(msg = txt, "coinbase error");
            return Step::Resync("server error");
        }
        let Some(seq) = seq else { return Step::Ignore };
        if let Some(p) = self.prev_seq
            && seq != p + 1
        {
            return Step::Resync("sequence gap");
        }
        self.prev_seq = Some(seq);
        if events.is_empty() {
            return Step::Ignore;
        }
        match channel {
            "l2_data" => {
                let mut de = serde_json::Deserializer::from_str(events);
                let mut ev = L2Events {
                    book,
                    snapshot: false,
                    ok: true,
                    last_time: "",
                };
                if ev.deserialize_all(&mut de).is_err() || !ev.ok {
                    return Step::Resync("malformed l2 event");
                }
                if ev.snapshot {
                    self.synced = true;
                }
                if !self.synced {
                    return Step::Ignore;
                }
                self.event_ms = rfc3339_ms(ev.last_time)
                    .or_else(|| rfc3339_ms(timestamp))
                    .unwrap_or(0);
                Step::Book
            }
            "market_trades" => {
                let Ok(NewestTrade(Some(t))) = serde_json::from_str::<NewestTrade>(events) else {
                    return Step::Ignore;
                };
                match (self.scale.parse(t.price), t.size.parse()) {
                    (Some(px), Ok(q)) => {
                        tape.trade(px, q, t.side == "SELL", rfc3339_ms(t.time).unwrap_or(0));
                        Step::Tape
                    }
                    _ => Step::Ignore,
                }
            }
            _ => Step::Ignore,
        }
    }
}

/// Streams `events[].updates[]` straight into the book. A `snapshot` event
/// clears the book first.
struct L2Events<'b, 'a> {
    book: &'b mut Book,
    snapshot: bool,
    ok: bool,
    last_time: &'a str,
}

impl<'b, 'a> L2Events<'b, 'a> {
    fn deserialize_all(
        &mut self,
        de: &mut serde_json::Deserializer<serde_json::de::StrRead<'a>>,
    ) -> serde_json::Result<()> {
        serde::Deserializer::deserialize_seq(de, EventsVisitor(self))
    }
}

struct EventsVisitor<'r, 'b, 'a>(&'r mut L2Events<'b, 'a>);

impl<'de: 'a, 'a> Visitor<'de> for EventsVisitor<'_, '_, 'a> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("l2 events")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<(), A::Error> {
        while s.next_element_seed(EventSeed(&mut *self.0))?.is_some() {}
        Ok(())
    }
}

struct EventSeed<'r, 'b, 'a>(&'r mut L2Events<'b, 'a>);

impl<'de: 'a, 'a> DeserializeSeed<'de> for EventSeed<'_, '_, 'a> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_map(self)
    }
}

impl<'de: 'a, 'a> Visitor<'de> for EventSeed<'_, '_, 'a> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("l2 event")
    }
    fn visit_map<M: MapAccess<'de>>(self, mut m: M) -> Result<(), M::Error> {
        // Coinbase sends "type" before "updates"; a snapshot must clear the
        // book before its levels go in.
        while let Some(k) = m.next_key::<&'de str>()? {
            match k {
                "type" => {
                    if m.next_value::<&'de str>()? == "snapshot" {
                        self.0.book.clear();
                        self.0.snapshot = true;
                    }
                }
                "updates" => m.next_value_seed(UpdatesSeed(&mut *self.0))?,
                _ => {
                    m.next_value::<IgnoredAny>()?;
                }
            }
        }
        Ok(())
    }
}

struct UpdatesSeed<'r, 'b, 'a>(&'r mut L2Events<'b, 'a>);

#[derive(Deserialize)]
struct Update<'a> {
    side: &'a str,
    event_time: &'a str,
    price_level: &'a str,
    new_quantity: &'a str,
}

impl<'de: 'a, 'a> DeserializeSeed<'de> for UpdatesSeed<'_, '_, 'a> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
        d.deserialize_seq(self)
    }
}

impl<'de: 'a, 'a> Visitor<'de> for UpdatesSeed<'_, '_, 'a> {
    type Value = ();
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("l2 updates")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<(), A::Error> {
        let ev = self.0;
        while let Some(u) = s.next_element::<Update<'de>>()? {
            let kind = if u.side == "bid" {
                Kind::Bid
            } else {
                Kind::Ask
            };
            ev.ok &= ev.book.set_str(kind, u.price_level, u.new_quantity);
            ev.last_time = u.event_time;
        }
        Ok(())
    }
}

pub async fn list_instruments(http: &reqwest::Client) -> Result<Vec<Instrument>> {
    #[derive(Deserialize)]
    struct Resp {
        products: Vec<Product>,
    }
    #[derive(Deserialize)]
    struct Product {
        product_id: String,
        base_currency_id: String,
        quote_currency_id: String,
        quote_increment: String,
        base_increment: String,
        status: String,
        #[serde(default)]
        trading_disabled: bool,
    }
    let resp: Resp = http
        .get(format!("{REST}/api/v3/brokerage/market/products"))
        .query(&[("product_type", "SPOT")])
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("decoding products")?;
    Ok(resp
        .products
        .into_iter()
        .map(|p| Instrument {
            symbol: p.product_id,
            base: p.base_currency_id,
            quote: p.quote_currency_id,
            tick: p.quote_increment,
            lot: p.base_increment,
            status: if p.trading_disabled {
                "disabled".into()
            } else {
                p.status
            },
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed() -> (Feed, Book, Tape) {
        let scale = PriceScale::from_tick("0.01").unwrap();
        (
            Feed::new("BTC-USD", scale),
            Book::new(scale),
            Tape::default(),
        )
    }

    fn l2(seq: u64, kind: &str, ups: &[(&str, &str, &str)]) -> String {
        let ups: Vec<_> = ups
            .iter()
            .map(|(s, p, q)| {
                format!(r#"{{"side":"{s}","event_time":"2026-10-10T18:22:16.952232Z","price_level":"{p}","new_quantity":"{q}"}}"#)
            })
            .collect();
        format!(
            r#"{{"channel":"l2_data","timestamp":"2026-10-10T18:22:17.320496592Z","sequence_num":{seq},"events":[{{"type":"{kind}","product_id":"BTC-USD","updates":[{}]}}]}}"#,
            ups.join(",")
        )
    }

    #[test]
    fn snapshot_updates_and_gap() {
        let (mut f, mut b, mut t) = feed();
        let snap = l2(
            0,
            "snapshot",
            &[
                ("bid", "100", "1"),
                ("offer", "100.5", "2"),
                ("bid", "99.99", "3"),
            ],
        );
        assert_eq!(f.on_text(&snap, &mut b, &mut t), Step::Book);
        assert!(f.is_synced());
        assert_eq!(b.best_bid().unwrap().px, 10000);
        assert_eq!(b.best_ask().unwrap().px, 10050);
        assert_eq!(f.event_ms, 1_791_656_536_952);
        assert_eq!(
            f.on_text(&l2(1, "update", &[("bid", "100", "0")]), &mut b, &mut t),
            Step::Book
        );
        assert_eq!(b.best_bid().unwrap().px, 9999);
        let hb = r#"{"channel":"heartbeats","timestamp":"x","sequence_num":2,"events":[{"current_time":"x","heartbeat_counter":3}]}"#;
        assert_eq!(f.on_text(hb, &mut b, &mut t), Step::Ignore);
        assert_eq!(
            f.on_text(&l2(4, "update", &[("bid", "1", "1")]), &mut b, &mut t),
            Step::Resync("sequence gap")
        );
    }

    #[test]
    fn trade_side_is_inverted_maker_side() {
        let (mut f, mut b, mut t) = feed();
        let tr = r#"{"channel":"market_trades","timestamp":"x","sequence_num":0,"events":[{"type":"update","trades":[{"product_id":"BTC-USD","trade_id":"1","price":"82951.14","size":"0.5","time":"2026-10-10T18:22:15.329367Z","side":"BUY"},{"product_id":"BTC-USD","trade_id":"0","price":"82951.15","size":"0.1","time":"2026-10-10T18:22:15.1Z","side":"SELL"}]}]}"#;
        assert_eq!(f.on_text(tr, &mut b, &mut t), Step::Tape);
        assert_eq!(t.last_px, Some(8_295_114));
        assert!(!t.last_buy, "BUY maker means the aggressor sold");
        assert_eq!(t.last_qty, 0.5);
    }
}
